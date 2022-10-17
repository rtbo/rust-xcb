macro_rules! link_struct {
    { $struct_name:ident, $lib_name:expr, [$($dylib_name:expr),*],
        $($field_name:ident: fn $fn_name:ident ($($param_name:ident : $param_type:ty),*) -> $ret_type:ty,)*
    } => {
        pub(crate) struct $struct_name {
            #[cfg(feature = "dl")]
            lib: crate::dl::Lib,

            $(pub(crate) $field_name: unsafe extern "C" fn ($($param_type, )*) -> $ret_type,)*
        }

        unsafe impl Send for $struct_name {}
        unsafe impl Sync for $struct_name {}

        // #[cfg(not(feature = "dl"))]
        // #[link(name = $lib_name)]
        // extern "C" {
        //     $(
        //         pub(crate) fn $fn_name ($($param_name: $param_type, )*) -> $ret_type;
        //     )*
        // }

        impl $struct_name {
            #[cfg(feature = "dl")]
            pub(crate) fn open () -> Result<$struct_name, $crate::dl::Error> {
                let lib = crate::dl::Lib::open_multi(&[$($dylib_name, )*])?;
                unsafe {
                    $(
                        let $fn_name = std::mem::transmute::<
                            _,
                            unsafe extern "C" fn ($($param_name: $param_type, )*) -> $ret_type
                        >(lib.symbol(stringify!($fn_name))?);
                    )*
                }
                Ok($struct_name {
                    lib: lib,
                    $(
                        $field_name: $fn_name,
                    )*
                })
            }

            #[cfg(not(feature = "dl"))]
            pub(crate) fn open () -> $struct_name {
                $struct_name {
                    $( $field_name: $fn_name, )*
                }
            }
        }
    };
}

pub(crate) use link_struct;
