pub trait FromSlice {
    fn from_le_slice(slice: &[u8]) -> Self;
    fn from_be_slice(slice: &[u8]) -> Self;
}

macro_rules! impl_from_slice {
    ($($t:ty),*) => {
        $(
            impl FromSlice for $t {
                fn from_le_slice(slice: &[u8]) -> Self {
                    Self::from_le_bytes(slice.try_into().expect("Incorrect length"))
                }

                fn from_be_slice(slice: &[u8]) -> Self {
                    Self::from_le_bytes(slice.try_into().expect("Incorrect length"))
                }
            }
        )*
    };
}

impl_from_slice!(u8, u16, u32, u64, i8, i16, i32, i64);
