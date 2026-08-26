extern crate proc_macro;

use syn::{parse_macro_input, DeriveInput};

mod value_type;

#[proc_macro_derive(ValueType)]
pub fn derive_value_type(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match value_type::impl_value_type(&input) {
        Ok(generated) => generated.into(),
        Err(error) => error.to_compile_error().into(),
    }
}
