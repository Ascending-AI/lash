//! Saved callable migration follows an explicit Link, preserving captured environments.
use crate::DocumentRefusal;
use lash_kernel_dialect::SavedFunction;
use lash_kernel_doc::FunctionValue;
use {lash_kernel_value::names::*, lash_kernel_value::package::Link};
/// Only a fully validated successor Link may replace a callable origin.
pub fn saved_function(
    function: &SavedFunction,
    link: &Link,
    function_id: FunctionId,
) -> Result<SavedFunction, DocumentRefusal> {
    if link.document != function.document.id()
        || !function.document.functions.contains_key(&function_id)
    {
        return Err(DocumentRefusal::FormNotCarried {
            site: lash_kernel_value::sites::Site::new(
                lash_kernel_value::sites::Unit::Library(function.callable.function),
                [],
            ),
            detail: "explicit successor callable is absent".into(),
        });
    }
    let mut carried = function.clone();
    carried.link = link.identity();
    carried.callable = CallableId {
        link: carried.link,
        function: function_id,
    };
    Ok(carried)
}
pub fn function_value(value: &FunctionValue, callable: CallableId) -> FunctionValue {
    let mut carried = value.clone();
    carried.callable = callable;
    carried
}
