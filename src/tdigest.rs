
use std::{
    convert::TryInto,
    cmp::min,
    mem::replace,
    slice,
};

use serde::{Serialize, Deserialize};

use pgx::*;
use pg_sys::Datum;

use flat_serialize::*;

use crate::palloc::{Internal, in_memory_context};
use crate::aggregate_utils::{aggregate_mctx, in_aggregate_context};

use tdigest::{
    TDigest,
    Centroid,
};

#[derive(Serialize, Deserialize, Clone)]
pub struct TDigestTransState {
    #[serde(skip_serializing)]
    buffer: Vec<f64>,
    digested: TDigest,
}

impl TDigestTransState {
    fn push(&mut self, value: f64) {
        self.buffer.push(value);
        if self.buffer.len() >= self.digested.max_size() {
            self.digest()
        }
    }

    fn digest(&mut self) {
        if self.buffer.is_empty() {
            return
        }
        let new = replace(&mut self.buffer, vec![]);
        self.digested = self.digested.merge_unsorted(new)
    }
}

#[allow(non_camel_case_types)]
type int = u32;

#[pg_extern]
pub fn tdigest_trans(
    state: Option<Internal<TDigestTransState>>,
    size: int,
    value: Option<f64>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<Internal<TDigestTransState>> {
    let mctx = aggregate_mctx(fcinfo);
    let mctx = match mctx {
        None => pgx::error!("cannot call as non-aggregate"),
        Some(mctx) => mctx,
    };
    unsafe {
        in_memory_context(mctx, || {
            let value = match value {
                None => return state,
                Some(value) => value,
            };
            let mut state = match state {
                None => TDigestTransState{
                    buffer: vec![],
                    digested: TDigest::new_with_size(size as _),
                }.into(),
                Some(state) => state,
            };
            state.push(value);
            Some(state)
        })
    }
}

#[pg_extern]
pub fn tdigest_combine(
    state1: Option<Internal<TDigestTransState>>,
    state2: Option<Internal<TDigestTransState>>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<Internal<TDigestTransState>> {
    let mctx = aggregate_mctx(fcinfo);
    let mctx = match mctx {
        None => pgx::error!("cannot call as non-aggregate"),
        Some(mctx) => mctx,
    };
    unsafe {
        in_memory_context(mctx, || {
            match (state1, state2) {
                (None, None) => None,
                (None, Some(state2)) => Some(state2.clone().into()),
                (Some(state1), None) => Some(state1.clone().into()),
                (Some(state1), Some(state2)) => {
                    let digvec = vec![state1.digested.clone(), state2.digested.clone()];
                    if !state1.buffer.is_empty() {
                        digvec[0].merge_unsorted(state1.buffer.clone());  // merge_unsorted should take a reference
                    }
                    if !state2.buffer.is_empty() {
                        digvec[1].merge_unsorted(state2.buffer.clone());
                    }

                    Some(TDigestTransState {
                            buffer: vec![],
                            digested: TDigest::merge_digests(digvec),
                        }.into()
                    )
                }
            }
        })
    }
}

#[allow(non_camel_case_types)]
type bytea = pg_sys::Datum;

#[pg_extern]
pub fn tdigest_serialize(
    mut state: Internal<TDigestTransState>,
) -> bytea {
    state.digest();
    let size = bincode::serialized_size(&*state)
        .unwrap_or_else(|e| pgx::error!("serialization error {}", e));
    let mut bytes = Vec::with_capacity(size as usize + 4);
    let mut varsize = [0; 4];
    unsafe {
        pgx::set_varsize(&mut varsize as *mut _ as *mut _, size as _);
    }
    bytes.extend_from_slice(&varsize);
    bincode::serialize_into(&mut bytes, &*state)
        .unwrap_or_else(|e| pgx::error!("serialization error {}", e));
    bytes.as_mut_ptr() as pg_sys::Datum
}

#[pg_extern]
pub fn tdigest_deserialize(
    bytes: bytea,
    _internal: Option<Internal<()>>,
) -> Internal<TDigestTransState> {
    let tdigest: TDigestTransState = unsafe {
        let detoasted = pg_sys::pg_detoast_datum(bytes as *mut _);
        let len = pgx::varsize_any_exhdr(detoasted);
        let data = pgx::vardata_any(detoasted);
        let bytes = slice::from_raw_parts(data as *mut u8, len);
        bincode::deserialize(bytes).unwrap_or_else(|e|
            pgx::error!("deserialization error {}", e))
    };
    tdigest.into()
}

crate::pg_type! {
    struct TimescaleTDigest: TsTDigestData {
        buckets: u32,
        count: u32,
        sum: f64,
        min: f64,
        max: f64,
        means: [f64; std::cmp::min(self.buckets, self.count)],
        weights: [u32; std::cmp::min(self.buckets, self.count)],
    }
}

impl<'input> TimescaleTDigest<'input> {
    fn to_tdigest(&self) -> TDigest {
        let size = min(*self.0.buckets, *self.0.count) as usize;
        let mut cents: Vec<Centroid> = Vec::new();

        for i in 0..size {
            cents.push(Centroid::new(self.0.means[i], self.0.weights[i] as f64));
        }

        TDigest::new(cents, *self.0.sum, *self.0.count as f64, *self.0.max, *self.0.min, *self.0.buckets as usize)
    }
}

impl<'input> InOutFuncs for TimescaleTDigest<'input> {
    fn output(&self, buffer: &mut StringInfo) {
        use std::io::Write;
        // for output we'll just write the debug format of the data
        // if we decide to go this route we'll probably automate this process
        //let _ = write!(buffer, "{:?}", self.0.data);
        let _ = write!(buffer, "TODO, this");
    }

    fn input(_input: &std::ffi::CStr) -> Self
    where
        Self: Sized,
    {
        unimplemented!("we don't bother implementing string input")
    }
}

#[pg_extern]
fn tdigest_final(
    state: Option<Internal<TDigestTransState>>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<TimescaleTDigest<'static>> {
    unsafe {
        in_aggregate_context(fcinfo, || {
            let mut state = match state {
                None => return None,
                Some(state) => state,
            };
            state.digest();

            let buckets : u32 = state.digested.max_size().try_into().unwrap();
            let count = state.digested.count() as u32;
            let vec_size = min(buckets as usize, count as usize);
            let mut means = vec!(0.0; vec_size);
            let mut weights = vec!(0; vec_size);

            for (i, cent) in state.digested.raw_centroids().iter().enumerate() {
                means[i] = cent.mean();
                weights[i] = cent.weight() as u32;
            }

            // we need to flatten the vector to a single buffer that contains
            // both the size, the data, and the varlen header
            let flattened = crate::flatten! {
                TsTDigestData{
                    header: &0,
                    buckets: &buckets,
                    count: &count,
                    sum: &state.digested.sum(),
                    min: &state.digested.min(),
                    max: &state.digested.max(),
                    means: &means,
                    weights: &weights,
                }
            };

            TimescaleTDigest(flattened).into()
        })
    }
}

#[pg_extern]
pub fn tdigest_quantile(
    digest: TimescaleTDigest,
    quantile: f64,
    _fcinfo: pg_sys::FunctionCallInfo,
) -> f64 {
    digest.to_tdigest().estimate_quantile(quantile)
}

#[pg_extern]
pub fn tdigest_quantile_at_value(
    digest: TimescaleTDigest,
    value: f64,
    _fcinfo: pg_sys::FunctionCallInfo,
) -> f64 {
    digest.to_tdigest().estimate_quantile_at_value(value)
}

#[pg_extern]
pub fn tdigest_count(
    digest: TimescaleTDigest,
    _fcinfo: pg_sys::FunctionCallInfo,
) -> f64 {
    *digest.0.count as f64
}

#[pg_extern]
pub fn tdigest_min(
    digest: TimescaleTDigest,
    _fcinfo: pg_sys::FunctionCallInfo,
) -> f64 {
    *digest.0.min
}

#[pg_extern]
pub fn tdigest_max(
    digest: TimescaleTDigest,
    _fcinfo: pg_sys::FunctionCallInfo,
) -> f64 {
    *digest.0.max
}

#[pg_extern]
pub fn tdigest_mean(
    digest: TimescaleTDigest,
    _fcinfo: pg_sys::FunctionCallInfo,
) -> f64 {
    if *digest.0.count > 0 {
        *digest.0.sum / *digest.0.count as f64
    } else {
        0.0
    }
}

#[pg_extern]
pub fn tdigest_sum(
    digest: TimescaleTDigest,
    _fcinfo: pg_sys::FunctionCallInfo,
) -> f64 {
    *digest.0.sum
}

#[cfg(any(test, feature = "pg_test"))]
mod tests {
    use pgx::*;

    fn apx_eql(value: f64, expected: f64, error: f64) {
        assert!((value - expected).abs() < error, "Float value {} differs from expected {} by more than {}", value, expected, error);
    }

    fn pct_eql(value: f64, expected: f64, pct_error: f64) {
        apx_eql(value, expected, pct_error * expected);
    }

    #[pg_test]
    fn test_aggregate() {
        Spi::execute(|client| {
            client.select("CREATE TABLE test (data DOUBLE PRECISION)", None, None);
            client.select("INSERT INTO test SELECT generate_series(0.01, 100, 0.01)", None, None);

            let sanity = client
                .select("SELECT COUNT(*) FROM test", None, None)
                .first()
                .get_one::<i32>();
            assert_eq!(10000, sanity.unwrap());

            client.select("CREATE VIEW digest AS SELECT t_digest(100, data) FROM test", None, None);
            let (min, max, count) = client
                .select("SELECT tdigest_min(t_digest), tdigest_max(t_digest), tdigest_count(t_digest) FROM digest", None, None)
                .first()
                .get_three::<f64, f64, f64>();

            apx_eql(min.unwrap(), 0.01, 0.000001);
            apx_eql(max.unwrap(), 100.0, 0.000001);
            apx_eql(count.unwrap(), 10000.0, 0.000001);

            let (mean, sum) = client
                .select("SELECT tdigest_mean(t_digest), tdigest_sum(t_digest) FROM digest", None, None)
                .first()
                .get_two::<f64, f64>();

            apx_eql(mean.unwrap(), 50.005, 0.0001);
            apx_eql(sum.unwrap(), 500050.0, 0.0001);

            for i in 0..=100 {
                let value = i as f64;
                let quantile = value / 100.0;

                let (est_val, est_quant) = client
                    .select(&format!("SELECT tdigest_quantile(t_digest, {}), tdigest_quantile_at_value(t_digest, {}) FROM digest", quantile, value), None, None)
                    .first()
                    .get_two::<f64, f64>();

                if i == 0 {
                    pct_eql(est_val.unwrap(), 0.01, 1.0);
                    apx_eql(est_quant.unwrap(), quantile, 0.0001);
                } else {
                    pct_eql(est_val.unwrap(), value, 1.0);
                    pct_eql(est_quant.unwrap(), quantile, 1.0);
                }
            }
        });
    }
}
