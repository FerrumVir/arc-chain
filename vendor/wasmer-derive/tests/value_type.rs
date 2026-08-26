use std::mem::{size_of, MaybeUninit};

use wasmer_derive::ValueType;
use wasmer_types::ValueType as _;

#[repr(C)]
#[derive(Clone, Copy, ValueType)]
struct Padded {
    first: u8,
    second: u32,
}

#[test]
fn valid_expansion_preserves_fields_and_zeroes_padding() {
    assert_eq!(size_of::<Padded>(), 8);

    let value = Padded {
        first: 7,
        second: 9,
    };
    let mut bytes = [MaybeUninit::new(0xa5_u8); size_of::<Padded>()];
    value.zero_padding_bytes(&mut bytes);

    let initialized = bytes.map(|byte| {
        // Every element starts initialized and the generated implementation
        // only replaces padding elements with another initialized byte.
        unsafe { byte.assume_init() }
    });

    assert_eq!(&initialized[1..4], &[0, 0, 0]);
    assert_eq!(initialized[0], 0xa5);
    assert_eq!(&initialized[4..8], &[0xa5; 4]);
}
