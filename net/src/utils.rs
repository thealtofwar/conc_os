pub trait FromSlice {
    fn from_le_slice(slice: &[u8]) -> Self;
    fn from_be_slice(slice: &[u8]) -> Self;
    fn from_ne_slice(slice: &[u8]) -> Self;
}

macro_rules! impl_from_slice {
    ($($t:ty),*) => {
        $(
            impl FromSlice for $t {
                fn from_le_slice(slice: &[u8]) -> Self {
                    Self::from_le_bytes(slice.try_into().expect("Incorrect length"))
                }

                fn from_be_slice(slice: &[u8]) -> Self {
                    Self::from_be_bytes(slice.try_into().expect("Incorrect length"))
                }

                fn from_ne_slice(slice: &[u8]) -> Self {
                    Self::from_ne_bytes(slice.try_into().expect("Incorrect length"))
                }
            }
        )*
    };
}

impl_from_slice!(u8, u16, u32, u64, i8, i16, i32, i64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_correct_endian() {
        let arr = [123, 231];
        assert_eq!(u16::from_le_slice(&arr), u16::from_le_bytes(arr));
        assert_eq!(u16::from_be_slice(&arr), u16::from_be_bytes(arr));
        assert_eq!(u16::from_ne_slice(&arr), u16::from_ne_bytes(arr));
    }

    #[test]
    fn native_matches_target_endian() {
        let arr = [0x12, 0x34, 0x56, 0x78];
        if cfg!(target_endian = "little") {
            assert_eq!(u32::from_ne_slice(&arr), 0x7856_3412);
        } else {
            assert_eq!(u32::from_ne_slice(&arr), 0x1234_5678);
        }
    }

    #[test]
    fn native_all_widths() {
        assert_eq!(u8::from_ne_slice(&[0xAB]), 0xAB);
        assert_eq!(i8::from_ne_slice(&[0xFF]), -1);

        let arr = [0xFF; 8];
        assert_eq!(u16::from_ne_slice(&arr[..2]), u16::MAX);
        assert_eq!(u32::from_ne_slice(&arr[..4]), u32::MAX);
        assert_eq!(u64::from_ne_slice(&arr[..8]), u64::MAX);
        assert_eq!(i16::from_ne_slice(&arr[..2]), -1);
        assert_eq!(i32::from_ne_slice(&arr[..4]), -1);
        assert_eq!(i64::from_ne_slice(&arr[..8]), -1);
    }

    #[test]
    fn native_roundtrips_through_bytes() {
        let value = 0x0123_4567_89AB_CDEFu64;
        assert_eq!(u64::from_ne_slice(&value.to_ne_bytes()), value);

        let value = -0x1234_5678i32;
        assert_eq!(i32::from_ne_slice(&value.to_ne_bytes()), value);
    }

    #[test]
    fn native_reads_subslice() {
        let buf = [0xDE, 0xAD, 0x12, 0x34, 0xBE, 0xEF];
        assert_eq!(
            u16::from_ne_slice(&buf[2..4]),
            u16::from_ne_bytes([0x12, 0x34])
        );
    }

    #[test]
    #[should_panic(expected = "Incorrect length")]
    fn native_panics_on_short_slice() {
        u32::from_ne_slice(&[0x01, 0x02]);
    }

    #[test]
    #[should_panic(expected = "Incorrect length")]
    fn native_panics_on_long_slice() {
        u32::from_ne_slice(&[0x01, 0x02, 0x03, 0x04, 0x05]);
    }
}
