//! The aggregates of PostgreSQL's `pg_aggregate` that the frontend has.

/// A row of `pg_aggregate`. `oid` is the `pg_proc` oid of the aggregate, a
/// type is the `oid` of its `pg_type` row, and `sort_operator` is 0 when the
/// aggregate has none.
pub(crate) struct Aggregate {
    pub oid: i64,
    pub kind: &'static str,
    pub direct_args: i64,
    pub final_extra: bool,
    pub moving_final_extra: bool,
    pub final_modify: &'static str,
    pub moving_final_modify: &'static str,
    pub sort_operator: i64,
    pub state_type: i64,
    pub state_space: i64,
    pub moving_state_type: i64,
    pub moving_state_space: i64,
    pub initial_value: Option<&'static str>,
    pub moving_initial_value: Option<&'static str>,
}

macro_rules! aggregates {
    ($((
        $oid:literal, $kind:literal, $direct_args:literal, $final_extra:literal,
        $moving_final_extra:literal, $final_modify:literal, $moving_final_modify:literal,
        $sort_operator:literal, $state_type:literal, $state_space:literal,
        $moving_state_type:literal, $moving_state_space:literal, $initial_value:expr,
        $moving_initial_value:expr
    ),)*) => {
        &[$(Aggregate {
            oid: $oid,
            kind: $kind,
            direct_args: $direct_args,
            final_extra: $final_extra,
            moving_final_extra: $moving_final_extra,
            final_modify: $final_modify,
            moving_final_modify: $moving_final_modify,
            sort_operator: $sort_operator,
            state_type: $state_type,
            state_space: $state_space,
            moving_state_type: $moving_state_type,
            moving_state_space: $moving_state_space,
            initial_value: $initial_value,
            moving_initial_value: $moving_initial_value,
        }),*]
    };
}

/// The rows as PostgreSQL 18 has them: aggfnoid, aggkind, aggnumdirectargs,
/// aggfinalextra, aggmfinalextra, aggfinalmodify, aggmfinalmodify, aggsortop,
/// aggtranstype, aggtransspace, aggmtranstype, aggmtransspace, agginitval
/// and aggminitval.
#[rustfmt::skip]
pub(crate) const AGGREGATES: &[Aggregate] = aggregates![
    (2100, "n", 0, false, false, "r", "r", 0, 2281, 48, 2281, 48, None, None),
    (2101, "n", 0, false, false, "r", "r", 0, 1016, 0, 1016, 0, Some("{0,0}"), Some("{0,0}")),
    (2102, "n", 0, false, false, "r", "r", 0, 1016, 0, 1016, 0, Some("{0,0}"), Some("{0,0}")),
    (2103, "n", 0, false, false, "r", "r", 0, 2281, 128, 2281, 128, None, None),
    (2104, "n", 0, false, false, "r", "r", 0, 1022, 0, 0, 0, Some("{0,0,0}"), None),
    (2105, "n", 0, false, false, "r", "r", 0, 1022, 0, 0, 0, Some("{0,0,0}"), None),
    (2107, "n", 0, false, false, "r", "r", 0, 2281, 48, 2281, 48, None, None),
    (2108, "n", 0, false, false, "r", "r", 0, 20, 0, 1016, 0, None, Some("{0,0}")),
    (2109, "n", 0, false, false, "r", "r", 0, 20, 0, 1016, 0, None, Some("{0,0}")),
    (2110, "n", 0, false, false, "r", "r", 0, 700, 0, 0, 0, None, None),
    (2111, "n", 0, false, false, "r", "r", 0, 701, 0, 0, 0, None, None),
    (2114, "n", 0, false, false, "r", "r", 0, 2281, 128, 2281, 128, None, None),
    (2115, "n", 0, false, false, "r", "r", 413, 20, 0, 0, 0, None, None),
    (2116, "n", 0, false, false, "r", "r", 521, 23, 0, 0, 0, None, None),
    (2117, "n", 0, false, false, "r", "r", 520, 21, 0, 0, 0, None, None),
    (2118, "n", 0, false, false, "r", "r", 610, 26, 0, 0, 0, None, None),
    (2119, "n", 0, false, false, "r", "r", 623, 700, 0, 0, 0, None, None),
    (2120, "n", 0, false, false, "r", "r", 674, 701, 0, 0, 0, None, None),
    (2122, "n", 0, false, false, "r", "r", 1097, 1082, 0, 0, 0, None, None),
    (2126, "n", 0, false, false, "r", "r", 2064, 1114, 0, 0, 0, None, None),
    (2127, "n", 0, false, false, "r", "r", 1324, 1184, 0, 0, 0, None, None),
    (2129, "n", 0, false, false, "r", "r", 666, 25, 0, 0, 0, None, None),
    (2130, "n", 0, false, false, "r", "r", 1756, 1700, 0, 0, 0, None, None),
    (2131, "n", 0, false, false, "r", "r", 412, 20, 0, 0, 0, None, None),
    (2132, "n", 0, false, false, "r", "r", 97, 23, 0, 0, 0, None, None),
    (2133, "n", 0, false, false, "r", "r", 95, 21, 0, 0, 0, None, None),
    (2134, "n", 0, false, false, "r", "r", 609, 26, 0, 0, 0, None, None),
    (2135, "n", 0, false, false, "r", "r", 622, 700, 0, 0, 0, None, None),
    (2136, "n", 0, false, false, "r", "r", 672, 701, 0, 0, 0, None, None),
    (2138, "n", 0, false, false, "r", "r", 1095, 1082, 0, 0, 0, None, None),
    (2142, "n", 0, false, false, "r", "r", 2062, 1114, 0, 0, 0, None, None),
    (2143, "n", 0, false, false, "r", "r", 1322, 1184, 0, 0, 0, None, None),
    (2145, "n", 0, false, false, "r", "r", 664, 25, 0, 0, 0, None, None),
    (2146, "n", 0, false, false, "r", "r", 1754, 1700, 0, 0, 0, None, None),
    (2147, "n", 0, false, false, "r", "r", 0, 20, 0, 20, 0, Some("0"), Some("0")),
    (2154, "n", 0, false, false, "r", "r", 0, 2281, 128, 2281, 128, None, None),
    (2155, "n", 0, false, false, "r", "r", 0, 2281, 48, 2281, 48, None, None),
    (2156, "n", 0, false, false, "r", "r", 0, 2281, 48, 2281, 48, None, None),
    (2157, "n", 0, false, false, "r", "r", 0, 1022, 0, 0, 0, Some("{0,0,0}"), None),
    (2158, "n", 0, false, false, "r", "r", 0, 1022, 0, 0, 0, Some("{0,0,0}"), None),
    (2159, "n", 0, false, false, "r", "r", 0, 2281, 128, 2281, 128, None, None),
    (2803, "n", 0, false, false, "r", "r", 0, 20, 0, 20, 0, Some("0"), Some("0")),
    (3538, "n", 0, false, false, "r", "r", 0, 2281, 0, 0, 0, None, None),
    (3545, "n", 0, false, false, "r", "r", 0, 2281, 0, 0, 0, None, None),
    (6395, "n", 0, false, false, "r", "r", 1959, 17, 0, 0, 0, None, None),
    (6396, "n", 0, false, false, "r", "r", 1957, 17, 0, 0, 0, None, None),
];
