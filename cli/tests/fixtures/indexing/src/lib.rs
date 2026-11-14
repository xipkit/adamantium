#![allow(dead_code)]

use adamantium::Array;

fn remove(values: &mut Vec<u8>, index: usize) {
    values.remove(index); // disallowed
    Vec::remove(values, index); // disallowed
}

fn insert(values: &mut Vec<u8>, index: usize) {
    values.insert(index, 0); // disallowed
    Vec::insert(values, index, 0); // disallowed
}

fn swap_remove(values: &mut Vec<u8>, index: usize) {
    values.swap_remove(index); // disallowed
    Vec::swap_remove(values, index); // disallowed
}

fn split_at(values: &[u8], index: usize) {
    let _ = values.split_at(index); // disallowed
    let _ = <[u8]>::split_at(values, index); // disallowed
}

fn copy_from_slice(values: &mut [u8], source: &[u8]) {
    values.copy_from_slice(source); // disallowed
    <[u8]>::copy_from_slice(values, source); // disallowed
}

fn checked_positions(values: &mut Array<u8>, index: usize) {
    let _ = values.remove(index);
    let _ = values.insert(index, 0);
    let _ = values.split_at_checked(index);
}

fn checked_copy(values: &mut [u8], source: &[u8]) -> Option<()> {
    if values.len() != source.len() {
        return None;
    }
    for (slot, value) in values.iter_mut().zip(source) {
        *slot = *value;
    }
    Some(())
}
