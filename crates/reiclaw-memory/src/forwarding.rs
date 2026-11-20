//! Crate-local syntax for behavior-free `Memory` delegation.
//!
//! The full impl is emitted before `async_trait` runs. Expanding async methods
//! from inside an already annotated impl would miss that transformation.
//! Policy, audit, scan, scope, and cache behavior stays in explicit methods at
//! each call site; this macro only forwards the same arguments to the same name.

macro_rules! forward_memory {
    (
        [$($implementation:tt)*]
        $inner:ident {
            $(
                async fn $method:ident(
                    &self $(, $arg:ident: $arg_type:ty)* $(,)?
                ) -> $return_type:ty;
            )*
        }
        $($owned:item)*
    ) => {
        #[async_trait::async_trait]
        $($implementation)* {
            $(
                async fn $method(&self $(, $arg: $arg_type)*) -> $return_type {
                    self.$inner.$method($($arg),*).await
                }
            )*
            $($owned)*
        }
    };
}

pub(crate) use forward_memory;
