//! Tiny parallel helpers: rayon on native when the work is big enough, sequential on wasm.

/// Minimum number of rows before row loops go parallel (native only).
pub const PAR_ROW_THRESHOLD: usize = 64;

/// Call `f(row_index, row)` for every `row_len`-sized chunk of `data`.
pub(crate) fn for_each_row<T: Send>(data: &mut [T], row_len: usize, f: impl Fn(usize, &mut [T]) + Sync + Send) {
    if row_len == 0 || data.is_empty() {
        return;
    }
    #[cfg(not(target_arch = "wasm32"))]
    if data.len() / row_len >= PAR_ROW_THRESHOLD {
        use rayon::prelude::*;
        data.par_chunks_mut(row_len).enumerate().for_each(|(y, r)| f(y, r));
        return;
    }
    data.chunks_mut(row_len).enumerate().for_each(|(y, r)| f(y, r));
}

/// Build a `rows × row_len` buffer by computing each row with `f(row_index, row)`.
pub(crate) fn build_rows<T: Send + Clone>(
    rows: usize,
    row_len: usize,
    fill: T,
    f: impl Fn(usize, &mut [T]) + Sync + Send,
) -> Vec<T> {
    let mut out = vec![fill; rows * row_len];
    for_each_row(&mut out, row_len, f);
    out
}

/// Map items, in parallel on native when there is more than one.
pub(crate) fn map<T: Send, R: Send>(items: Vec<T>, f: impl Fn(T) -> R + Sync + Send) -> Vec<R> {
    #[cfg(not(target_arch = "wasm32"))]
    if items.len() > 1 {
        use rayon::prelude::*;
        return items.into_par_iter().map(f).collect();
    }
    items.into_iter().map(f).collect()
}
