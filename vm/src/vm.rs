use std::collections::HashMap;

use log::{debug, error, info};
use typed_arena::Arena;

use rjvm_reader::{field_type::BaseType, line_number::LineNumber};
use rjvm_utils::type_conversion::ToUsizeSafe;

use crate::abstract_object::{string_from_char_array, AbstractObject, Array2, Object2, ObjectKind};
use crate::native_methods_impl::array_copy;
use crate::{
    array_entry_type::ArrayEntryType,
    call_frame::MethodCallResult,
    call_stack::CallStack,
    class::{ClassId, ClassRef},
    class_and_method::ClassAndMethod,
    class_manager::{ClassManager, ResolvedClass},
    class_path::ClassPathParseError,
    class_resolver_by_id::ClassByIdResolver,
    exceptions::MethodCallFailed,
    gc::ObjectAllocator,
    native_methods_registry::NativeMethodsRegistry,
    stack_trace_element::StackTraceElement,
    value::Value,
    vm_error::VmError,
};

pub struct Vm<'a> {
    /// Responsible for allocating and storing classes
    class_manager: ClassManager<'a>,

    /// Responsible for allocating objects
    object_allocator: ObjectAllocator<'a>,

    /// Allocated call stacks
    call_stacks: Arena<CallStack<'a>>,

    /// To model static fields, we will create one special instance of each class
    /// and we will store it in this map
    statics: HashMap<ClassId, AbstractObject<'a>>,

    /// Stores native methods
    pub native_methods_registry: NativeMethodsRegistry<'a>,

    /// Stores call stacks collected, and associate them with their throwable.
    /// In the classes that we are using, the Throwable implementation does not
    /// store the stack trace in the java fields, but rather relies on a native
    /// array. Since we have no place to store it inside the actual object, we will
    /// keep it in this weird map.
    /// See the implementation of Throwable::getStackTrace() in our rt.jar for
    /// clarity.
    throwable_call_stacks: HashMap<i32, Vec<StackTraceElement<'a>>>,

    pub printed: Vec<Value<'a>>, // Temporary, used for testing purposes
}

const ONE_MEGABYTE: usize = 1024 * 1024;
pub const DEFAULT_MAX_MEMORY: usize = 100 * ONE_MEGABYTE;

impl<'a> ClassByIdResolver<'a> for Vm<'a> {
    fn find_class_by_id(&self, class_id: ClassId) -> Option<ClassRef<'a>> {
        self.class_manager.find_class_by_id(class_id)
    }
}

impl<'a> Vm<'a> {
    pub fn new(max_memory: usize) -> Self {
        let mut result = Self {
            class_manager: Default::default(),
            object_allocator: ObjectAllocator::with_maximum_memory(max_memory),
            call_stacks: Arena::new(),
            statics: Default::default(),
            native_methods_registry: Default::default(),
            throwable_call_stacks: Default::default(),
            printed: Vec::new(),
        };
        crate::native_methods_impl::register_natives(&mut result.native_methods_registry);
        result
    }

    pub fn extract_str_from_java_lang_string(
        &self,
        object: &impl Object2<'a>,
    ) -> Result<String, VmError> {
        let class = self.get_class_by_id(object.class_id())?;
        if class.name == "java/lang/String" {
            // In our JRE's rt.jar, the first fields of String is
            //    private final char[] value;
            if let Value::Object(array) = object.get_field(class, 0) {
                return string_from_char_array(array);
            }
        }
        Err(VmError::ValidationException)
    }

    pub(crate) fn get_static_instance(&self, class_id: ClassId) -> Option<AbstractObject<'a>> {
        self.statics.get(&class_id).cloned()
    }

    pub fn append_class_path(&mut self, class_path: &str) -> Result<(), ClassPathParseError> {
        self.class_manager.append_class_path(class_path)
    }

    pub fn get_or_resolve_class(
        &mut self,
        stack: &mut CallStack<'a>,
        class_name: &str,
    ) -> Result<ClassRef<'a>, MethodCallFailed<'a>> {
        let class = self.class_manager.get_or_resolve_class(class_name)?;
        if let ResolvedClass::NewClass(classes_to_init) = &class {
            for class_to_init in classes_to_init.to_initialize.iter() {
                self.init_class(stack, class_to_init)?;
            }
        }
        Ok(class.get_class())
    }

    fn init_class(
        &mut self,
        stack: &mut CallStack<'a>,
        class_to_init: &ClassRef<'a>,
    ) -> Result<(), MethodCallFailed<'a>> {
        debug!("creating static instance of {}", class_to_init.name);
        let static_instance = self.new_object_of_class(class_to_init);
        self.statics.insert(class_to_init.id, static_instance);
        if let Some(clinit_method) = class_to_init.find_method("<clinit>", "()V") {
            debug!("invoking {}::<clinit>()", class_to_init.name);
            self.invoke(
                stack,
                ClassAndMethod {
                    class: class_to_init,
                    method: clinit_method,
                },
                None,
                Vec::new(),
            )?;
        }
        Ok(())
    }

    pub fn get_class_by_id(&self, class_id: ClassId) -> Result<ClassRef<'a>, VmError> {
        self.find_class_by_id(class_id)
            .ok_or(VmError::ValidationException)
    }

    pub fn find_class_by_name(&self, class_name: &str) -> Option<ClassRef<'a>> {
        self.class_manager.find_class_by_name(class_name)
    }

    pub fn resolve_class_method(
        &mut self,
        call_stack: &mut CallStack<'a>,
        class_name: &str,
        method_name: &str,
        method_type_descriptor: &str,
    ) -> Result<ClassAndMethod<'a>, MethodCallFailed<'a>> {
        self.get_or_resolve_class(call_stack, class_name)
            .and_then(|class| {
                class
                    .find_method(method_name, method_type_descriptor)
                    .map(|method| ClassAndMethod { class, method })
                    .ok_or(MethodCallFailed::InternalError(
                        VmError::MethodNotFoundException(
                            class_name.to_string(),
                            method_name.to_string(),
                            method_type_descriptor.to_string(),
                        ),
                    ))
            })
    }

    pub fn invoke(
        &mut self,
        call_stack: &mut CallStack<'a>,
        class_and_method: ClassAndMethod<'a>,
        object: Option<AbstractObject<'a>>,
        args: Vec<Value<'a>>,
    ) -> MethodCallResult<'a> {
        if class_and_method.method.is_native() {
            return self.invoke_native(call_stack, class_and_method, object, args);
        }

        let mut frame = call_stack.add_frame(class_and_method, object, args)?;
        let result = frame.as_mut().execute(self, call_stack);
        call_stack
            .pop_frame()
            .expect("should be able to pop the frame we just pushed");
        result
    }

    fn invoke_native(
        &mut self,
        call_stack: &mut CallStack<'a>,
        class_and_method: ClassAndMethod<'a>,
        object: Option<AbstractObject<'a>>,
        args: Vec<Value<'a>>,
    ) -> MethodCallResult<'a> {
        let native_callback = self.native_methods_registry.get_method(&class_and_method);
        if let Some(native_callback) = native_callback {
            debug!(
                "executing native method {}::{} {}",
                class_and_method.class.name,
                class_and_method.method.name,
                class_and_method.method.type_descriptor
            );
            native_callback(self, call_stack, object, args)
        } else {
            error!(
                "cannot resolve native method {}::{} {}",
                class_and_method.class.name,
                class_and_method.method.name,
                class_and_method.method.type_descriptor
            );
            Err(MethodCallFailed::InternalError(VmError::NotImplemented))
        }
    }

    pub fn allocate_call_stack(&mut self) -> &'a mut CallStack<'a> {
        let stack = self.call_stacks.alloc(CallStack::new());
        unsafe {
            let stack_ptr: *mut CallStack<'a> = stack;
            &mut *stack_ptr
        }
    }

    pub fn new_object(
        &mut self,
        call_stack: &mut CallStack<'a>,
        class_name: &str,
    ) -> Result<AbstractObject<'a>, MethodCallFailed<'a>> {
        let class = self.get_or_resolve_class(call_stack, class_name)?;
        Ok(self.new_object_of_class(class))
    }

    pub fn new_object_of_class(&mut self, class: ClassRef<'a>) -> AbstractObject<'a> {
        debug!("allocating new instance of {}", class.name);
        match self.object_allocator.allocate(class) {
            Some(object) => object,
            None => {
                self.do_garbage_collection();
                self.object_allocator
                    .allocate(class)
                    .expect("cannot allocate object even after full garbage collection!")
            }
        }
    }

    pub fn new_java_lang_string_object(
        &mut self,
        call_stack: &mut CallStack<'a>,
        string: &str,
    ) -> Result<AbstractObject<'a>, MethodCallFailed<'a>> {
        let char_array: Vec<Value<'a>> = string
            .encode_utf16()
            .map(|c| Value::Int(c as i32))
            .collect();

        let java_array = self.new_array(ArrayEntryType::Base(BaseType::Char), char_array.len());
        char_array
            .into_iter()
            .enumerate()
            .for_each(|(index, value)| java_array.set_element(index, value).unwrap());

        // In our JRE's rt.jar, the fields for String are:
        //    private final char[] value;
        //    private int hash;
        //    private static final long serialVersionUID = -6849794470754667710L;
        //    private static final ObjectStreamField[] serialPersistentFields = new ObjectStreamField[0];
        //    public static final Comparator<String> CASE_INSENSITIVE_ORDER = new CaseInsensitiveComparator();
        //    private static final int HASHING_SEED;
        //    private transient int hash32;
        let string_object = self.new_object(call_stack, "java/lang/String")?;
        string_object.set_field(0, Value::Object(java_array));
        string_object.set_field(1, Value::Int(0));
        string_object.set_field(6, Value::Int(0));
        Ok(string_object)
    }

    pub fn new_java_lang_class_object(
        &mut self,
        call_stack: &mut CallStack<'a>,
        class_name: &str,
    ) -> Result<AbstractObject<'a>, MethodCallFailed<'a>> {
        let class_object = self.new_object(call_stack, "java/lang/Class")?;
        // TODO: build a proper instance of Class object
        let string_object = Self::new_java_lang_string_object(self, call_stack, class_name)?;
        class_object.set_field(5, Value::Object(string_object));
        Ok(class_object)
    }

    pub fn new_java_lang_stack_trace_element_object(
        &mut self,
        call_stack: &mut CallStack<'a>,
        stack_trace_element: &StackTraceElement<'a>,
    ) -> Result<AbstractObject<'a>, MethodCallFailed<'a>> {
        let class_name = Value::Object(
            self.new_java_lang_string_object(call_stack, stack_trace_element.class_name)?,
        );
        let method_name = Value::Object(
            self.new_java_lang_string_object(call_stack, stack_trace_element.method_name)?,
        );
        let file_name = match stack_trace_element.source_file {
            Some(file_name) => {
                Value::Object(self.new_java_lang_string_object(call_stack, file_name)?)
            }
            _ => Value::Null,
        };
        let line_number =
            Value::Int(stack_trace_element.line_number.unwrap_or(LineNumber(0)).0 as i32);

        // The class StackTraceElement has this layout:
        //     private String declaringClass;
        //     private String methodName;
        //     private String fileName;
        //     private int    lineNumber;
        let stack_trace_element_java_object =
            self.new_object(call_stack, "java/lang/StackTraceElement")?;
        stack_trace_element_java_object.set_field(0, class_name);
        stack_trace_element_java_object.set_field(1, method_name);
        stack_trace_element_java_object.set_field(2, file_name);
        stack_trace_element_java_object.set_field(3, line_number);

        Ok(stack_trace_element_java_object)
    }

    pub fn new_array(
        &mut self,
        elements_type: ArrayEntryType,
        length: usize,
    ) -> AbstractObject<'a> {
        match self
            .object_allocator
            .allocate_array(elements_type.clone(), length)
        {
            Some(array) => array,
            None => {
                self.do_garbage_collection();
                self.object_allocator
                    .allocate_array(elements_type, length)
                    .expect("cannot allocate array even after full garbage collection!")
            }
        }
    }

    pub fn clone_array(&mut self, value: Value<'a>) -> Result<Value<'a>, VmError> {
        match &value {
            Value::Object(array) if array.kind() == ObjectKind::Array => {
                let new_array =
                    self.new_array(array.elements_type(), array.len().into_usize_safe());
                array_copy(array, 0, &new_array, 0, array.len().into_usize_safe())?;
                Ok(Value::Object(new_array))
            }
            _ => Err(VmError::ValidationException),
        }
    }

    pub(crate) fn associate_stack_trace_with_throwable(
        &mut self,
        throwable: AbstractObject<'a>,
        call_stack: Vec<StackTraceElement<'a>>,
    ) {
        self.throwable_call_stacks
            .insert(throwable.identity_hash_code(), call_stack);
    }

    pub(crate) fn get_stack_trace_associated_with_throwable(
        &self,
        throwable: AbstractObject<'a>,
    ) -> Option<&Vec<StackTraceElement<'a>>> {
        self.throwable_call_stacks
            .get(&throwable.identity_hash_code())
    }

    pub fn debug_stats(&self) {
        debug!(
            "VM classes={:?}, objects = {:?}",
            self.class_manager, self.object_allocator
        )
    }

    fn do_garbage_collection(&mut self) {
        info!("running garbage collection");

        let mut roots = vec![];
        roots.extend(
            self.statics
                .iter_mut()
                .map(|(_, object)| object as *mut AbstractObject<'a>),
        );
        roots.extend(self.call_stacks.iter_mut().flat_map(|s| s.gc_roots()));

        unsafe {
            self.object_allocator
                .do_garbage_collection(roots, &self.class_manager);
        }

        // todo!("implement garbage collection")
    }
}
