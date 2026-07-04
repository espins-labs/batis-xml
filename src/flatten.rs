//! Dynamic-tag flattening (MM-06).
//!
//! MyBatis: `<if>`, `<choose>/<when>/<otherwise>`, `<where>`, `<set>`,
//! `<trim>`, `<foreach>`, `<bind>`. iBatis: `<dynamic>`, `<isNotEmpty>`,
//! `<isEqual>`, `<iterate>`, … (MM-11).
//!
//! Rule: if the branch combination count (cartesian product of tag
//! branches) is at most [`BRANCH_LIMIT`], emit `SqlText::Variants`;
//! otherwise fall back to `SqlText::Union` (all-branch union text) plus a
//! `BranchLimitExceeded` diagnostic.
//!
//! Every segment of the produced text maps back to original byte offsets
//! (`SqlString::span_map`) so downstream SQL analysis can point at the XML
//! source.

/// Per-statement cap on flattened candidates (fixed by spec).
#[allow(dead_code)] // used once MM-06 lands
pub(crate) const BRANCH_LIMIT: u32 = 32;

// TODO(MM-06): dynamic node tree (from the tree builder) → SqlText.

#[cfg(test)]
mod tests {
    // mm_06_* snapshot tests (including the branch-explosion boundary:
    // 31/32/33 branches).
}
