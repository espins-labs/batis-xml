//! Placeholder normalization (MM-07).
//!
//! This crate is the **sole owner** of this responsibility (downstream SQL
//! analyzers only ever see normalized text):
//! - `#{expr}` / iBatis `#expr#`  → `?`
//! - `${expr}` / iBatis `$expr$`  → [`DYN_MARKER`]
//! - Property paths inside `expr` are collected separately into
//!   `Statement::property_paths`.
//! - Option syntax (`#{id, jdbcType=VARCHAR}`) keeps only the path.
//!
//! Must also work inside CDATA sections (combined with MM-08).

/// Substitution marker for `${}` dynamic fragments (fixed by spec).
#[allow(dead_code)] // used once MM-07 lands
pub(crate) const DYN_MARKER: &str = "__ATLAS_DYN__";

// TODO(MM-07): normalizer — updates input segments and span_map together.

#[cfg(test)]
mod tests {
    // mm_07_* table tests (nested braces, jdbcType options, legacy iBatis
    // `#..#` / `$..$` forms).
}
