use async_stream::stream;
use async_std::task;
use std::sync::Arc;

use futures_util::pin_mut;
use futures_util::stream::{Stream, StreamExt};
use std::io::Cursor;

pub fn xdr(f: String, vs: Vec<String>) -> impl Stream<Item = Vec<u8>> {
    debug!("XDR: {}:{:?}", f, vs);

    let f = f.to_string();
    // let v = f.to_string();

    stream! {
        // let nc = Arc::new(task::spawn_blocking(move || {
        //     netcdf::open(format!("data/{}", f)).expect("could not open file")
        // }).await);
        let nc = netcdf::open(format!("data/{}", f)).expect("could not open file");

        for v in vs {
            // let nnc = nc.clone();
            // let vbuf = task::spawn_blocking(move || {
                let vv = nc.variable(&v).expect("could not open variable");
                let mut vbuf: Vec<f64> = vec![0.0; vv.len()];
                vv.values_to(&mut vbuf, None, None).expect("could not read values");

                // vbuf
            // }).await;

            let mut buf: Cursor<Vec<u8>> = Cursor::new(Vec::new());
            use xdr_codec::pack;

            pack(&vbuf.len(), &mut buf).unwrap();
            xdr_codec::pack(&vbuf, &mut buf).expect("could not pack XDR");

            yield buf.into_inner();
        }
    }
}

// struct NcDods;

// constraints
//
// * variables
// * hyperslabs

// impl NcDods {
    // pub fn parse_hyberslab(q: &str) -> Vec<usize> {
    //     // [0:10][1:30]

    // }

    // Stream {
    //     async read; poll ready
    // }
// }
pub fn var_xdr(f: &str, v: &str) -> Vec<u8> {
    // XXX: Float32 is apparently f64 in xdr world.
    debug!("XDR: {}:{}", f, v);

    let f = netcdf::open(format!("data/{}", f)).expect("could not open file");

    let v = f.variable(v).expect("could not open variable");

    let mut vbuf: Vec<f64> = vec![0.0; v.len()];
    v.values_to(&mut vbuf, None, None).expect("could not read values");

    let mut buf: Cursor<Vec<u8>> = Cursor::new(Vec::new());
    use xdr_codec::pack;

    pack(&vbuf.len(), &mut buf).unwrap();
    xdr_codec::pack(&vbuf, &mut buf).expect("could not pack XDR");

    buf.into_inner()
}

