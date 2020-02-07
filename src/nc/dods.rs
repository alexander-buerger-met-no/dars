use async_stream::stream;
use byte_slice_cast::IntoByteVec;
use futures::pin_mut;
use futures::stream::{self, Stream, StreamExt};
use itertools::izip;
use std::cmp::min;
use std::pin::Pin;
use std::sync::Arc;

use crate::dap2::{
    dods::{self, StreamingDataset, XdrPack},
    hyperslab::{count_slab, parse_hyberslab},
};

use super::NcDataset;

impl StreamingDataset for NcDataset {
    fn get_var_size(&self, var: &str) -> Result<usize, anyhow::Error> {
        self.f
            .variable(var)
            .map(|v| v.dimensions().iter().map(|d| d.len()).product::<usize>())
            .ok_or_else(|| anyhow!("could not find variable"))
    }

    fn get_var_single_value(&self, var: &str) -> Result<bool, anyhow::Error> {
        self.f
            .variable(var)
            .map(|v| v.dimensions().is_empty())
            .ok_or_else(|| anyhow!("could not find variable"))
    }

    /// Stream a variable with a predefined chunk size. Chunk size is not guaranteed to be
    /// kept, and may be at worst half of specified size in order to fill up slabs.
    fn stream_variable<T>(
        &self,
        vn: &str,
        indices: Option<&[usize]>,
        counts: Option<&[usize]>,
    ) -> Pin<Box<dyn Stream<Item = Result<Vec<T>, anyhow::Error>> + Send + Sync + 'static>>
    where
        T: netcdf::Numeric + Unpin + Clone + std::default::Default + Send + Sync + 'static,
    {
        const CHUNK_SZ: usize = 10 * 1024 * 1024;

        let f = self.f.clone();
        let v = f.variable(&vn).unwrap();
        let counts: Vec<usize> = counts
            .map(|c| c.to_vec())
            .unwrap_or_else(|| v.dimensions().iter().map(|d| d.len()).collect());
        let indices: Vec<usize> = indices
            .map(|i| i.to_vec())
            .unwrap_or_else(|| vec![0usize; std::cmp::min(v.dimensions().len(), 1)]);

        Box::pin(stream! {
            let mut jump: Vec<usize> = counts.iter().rev().scan(1, |n, &c| {
                if *n >= CHUNK_SZ {
                    Some(1)
                } else {
                    let p = min(CHUNK_SZ / *n, c);
                    *n *= p;

                    Some(p)
                }
            }).collect::<Vec<usize>>();
            jump.reverse();

            // size of count dimensions
            let mut dim_sz: Vec<usize> = counts.iter().rev().scan(1, |p, &c| {
                let sz = *p;
                *p *= c;
                Some(sz)
            }).collect();
            dim_sz.reverse();

            let mut offset = vec![0usize; counts.len()];

            loop {
                let mjump: Vec<usize> = izip!(&offset, &jump, &counts)
                    .map(|(o, j, c)| if o + j > *c { *c - *o } else { *j }).collect();
                let jump_sz: usize = mjump.iter().product();

                let mind: Vec<usize> = indices.iter().zip(&offset).map(|(a,b)| a + b).collect();

                let mut buf: Vec<T> = vec![T::default(); jump_sz];
                v.values_to(&mut buf, Some(&mind), Some(&mjump))?;

                yield Ok(buf);

                // let f = f.clone();
                // let mvn = vn.clone();
                // let cache = tokio::task::block_in_place(|| {
                //     let mut cache: Vec<T> = vec![T::default(); jump_sz];
                //     let v = f.variable(&mvn).ok_or(anyhow!("Could not find variable"))?;

                //     v.values_to(&mut cache, Some(&mind), Some(&mjump))?;
                //     Ok::<_,anyhow::Error>(cache)
                // })?;

                // yield Ok(cache);

                let mut carry = offset.iter().zip(&dim_sz).map(|(a,b)| a * b).sum::<usize>() + jump_sz;
                for (o, c) in izip!(offset.iter_mut().rev(), counts.iter().rev()) {
                    *o = carry % *c;
                    carry /= c;
                }

                if carry > 0 {
                    break;
                }
            }
        })
    }

    fn stream_encoded_variable(
        &self,
        v: &str,
        indices: Option<&[usize]>,
        counts: Option<&[usize]>,
    ) -> Pin<Box<dyn Stream<Item = Result<Vec<u8>, anyhow::Error>> + Send + Sync + 'static>> {
        let vv = self.f.variable(&v).unwrap();
        match vv.vartype() {
            netcdf_sys::NC_FLOAT => self.stream_encoded_variable_impl::<f32>(v, indices, counts),
            netcdf_sys::NC_DOUBLE => self.stream_encoded_variable_impl::<f64>(v, indices, counts),
            netcdf_sys::NC_INT => self.stream_encoded_variable_impl::<i32>(v, indices, counts),
            netcdf_sys::NC_SHORT => self.stream_encoded_variable_impl::<i32>(v, indices, counts),
            netcdf_sys::NC_BYTE => self.stream_encoded_variable_impl::<u8>(v, indices, counts),
            _ => unimplemented!(),
        }
    }
}

/// This only picks the correct generic for variable type.
pub fn pack_var(
    f: Arc<netcdf::File>,
    v: String,
    len: Option<usize>,
    slab: (Vec<usize>, Vec<usize>),
) -> impl Stream<Item = Result<Vec<u8>, anyhow::Error>> {
    stream! {
        let vv = f.variable(&v).unwrap();
        let mut s = match vv.vartype() {
            netcdf_sys::NC_FLOAT => pack_var_impl::<f32>(f, v, len, slab),
            netcdf_sys::NC_DOUBLE => pack_var_impl::<f64>(f, v, len, slab),
            netcdf_sys::NC_INT => pack_var_impl::<i32>(f, v, len, slab),
            netcdf_sys::NC_SHORT => pack_var_impl::<i32>(f, v, len, slab),
            netcdf_sys::NC_BYTE => pack_var_impl::<u8>(f, v, len, slab),
            // netcdf_sys::NC_UBYTE => xdr_bytes(vv),
            // netcdf_sys::NC_CHAR => xdr_bytes(vv),
            _ => unimplemented!()
        };

        while let Some(i) = s.next().await {
            yield i
        }
    }
}

pub fn pack_var_impl<T>(
    f: Arc<netcdf::File>,
    v: String,
    len: Option<usize>,
    slab: (Vec<usize>, Vec<usize>),
) -> Pin<Box<dyn Stream<Item = Result<Vec<u8>, anyhow::Error>> + Send + Sync + 'static>>
where
    T: netcdf::variable::Numeric
        + Unpin
        + Sync
        + Send
        + 'static
        + std::default::Default
        + std::clone::Clone
        + std::fmt::Debug,
    [T]: XdrPack,
    Vec<T>: IntoByteVec,
{
    let vv = f.variable(&v).unwrap();
    let (indices, counts) = slab;

    if !vv.dimensions().is_empty() {
        let v = stream_variable::<T>(f, v, indices, counts);

        Box::pin(dods::encode_array(v, len))
    } else {
        let mut vbuf: Vec<T> = vec![T::default(); 1];
        match vv.values_to(&mut vbuf, None, None) {
            Ok(_) => Box::pin(stream::once(async move { dods::encode_value(vbuf) })),
            Err(e) => Box::pin(stream::once(async move { Err(e.into()) })),
        }
    }
}

pub fn xdr(
    nc: Arc<netcdf::File>,
    vs: Vec<String>,
) -> impl Stream<Item = Result<Vec<u8>, anyhow::Error>> {
    stream! {
        for v in vs {
            // TODO: Structures not supported, only single variables.

            let mut mv = match v.find(".") {
                Some(i) => &v[i+1..],
                None => &v
            };


            let nc = nc.clone();
            let (vv, indices, counts) = match mv.find("[") {
                Some(i) => {
                    let slab = parse_hyberslab(&mv[i..])?;
                    mv = &mv[..i];

                    let counts = slab.iter().map(|v| count_slab(&v)).collect::<Vec<usize>>();
                    let indices = slab.iter().map(|slab| slab[0]).collect::<Vec<usize>>();

                    if slab.iter().any(|s| s.len() > 2) {
                        yield Err(anyhow!("Strides not implemented yet"));
                    }

                    let vv = nc.variable(&mv).ok_or(anyhow!("variable not found"))?;
                    (vv, indices, counts)
                },

                None => {
                    let vv = nc.variable(&mv).ok_or(anyhow!("variable not found"))?;
                    let n = vv.dimensions().len();
                    let counts = vv.dimensions().iter().map(|d| d.len()).collect::<Vec<usize>>();
                    (vv, vec![0usize; n], counts)
                }
            };

            let slab = (indices, counts);

            let pack = pack_var(nc,
                String::from(mv),
                Some(slab.1.iter().product::<usize>()),
                slab);

            pin_mut!(pack);

            while let Some(p) = pack.next().await {
                yield p;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test::Bencher;

    #[bench]
    fn open_nc(b: &mut Bencher) {
        b.iter(|| netcdf::open("data/coads_climatology.nc").unwrap());
    }

    #[bench]
    fn open_nc_native(b: &mut Bencher) {
        use std::fs::File;

        b.iter(|| {
            let f = File::open("data/coads_climatology.nc").unwrap();

            f
        });
    }

    #[bench]
    fn read_native_all(b: &mut Bencher) {
        b.iter(|| std::fs::read("data/coads_climatology.nc").unwrap());
    }

    #[bench]
    fn read_var_preopen(b: &mut Bencher) {
        let f = netcdf::open("data/coads_climatology.nc").unwrap();
        b.iter(|| {
            let v = f.variable("SST").unwrap();

            let mut vbuf: Vec<f32> = vec![0.0; v.len()];
            v.values_to(&mut vbuf, None, None)
                .expect("could not read values");

            vbuf
        });
    }

    #[bench]
    fn read_var(b: &mut Bencher) {
        b.iter(|| {
            let f = netcdf::open("data/coads_climatology.nc").unwrap();
            let v = f.variable("SST").unwrap();

            let mut vbuf: Vec<f32> = vec![0.0; v.len()];
            v.values_to(&mut vbuf, None, None).unwrap();

            vbuf
        });
    }

    #[bench]
    fn xdr_stream(b: &mut Bencher) {
        use futures::executor::block_on_stream;
        use futures::pin_mut;

        let f = Arc::new(netcdf::open("data/coads_climatology.nc").unwrap());

        b.iter(|| {
            let f = f.clone();
            let v = xdr(f, vec!["SST".to_string()]);

            pin_mut!(v);
            block_on_stream(v).collect::<Vec<_>>()
        });
    }

    #[bench]
    fn xdr_stream_chunk(b: &mut Bencher) {
        use futures::executor::block_on_stream;

        let f = Arc::new(netcdf::open("data/coads_climatology.nc").unwrap());
        let counts: Vec<usize> = f
            .variable("SST")
            .unwrap()
            .dimensions()
            .iter()
            .map(|d| d.len())
            .collect();

        b.iter(|| {
            let f = f.clone();

            let v = stream_variable::<f32>(f, "SST".to_string(), vec![0, 0, 0], counts.clone());

            let x2 = dods::encode_array(v, Some(counts.iter().product::<usize>()));
            pin_mut!(x2);
            block_on_stream(x2).collect::<Vec<_>>()
        });
    }

    #[test]
    fn test_async_xdr_stream() {
        use futures::executor::block_on_stream;

        let f = Arc::new(netcdf::open("data/coads_climatology.nc").unwrap());

        let v = xdr(f.clone(), vec!["SST".to_string()]);

        pin_mut!(v);
        let x = block_on_stream(v).flatten().flatten().collect::<Vec<u8>>();

        let counts: Vec<usize> = f
            .variable("SST")
            .unwrap()
            .dimensions()
            .iter()
            .map(|d| d.len())
            .collect();
        let v = stream_variable::<f32>(f, "SST".to_string(), vec![0, 0, 0], counts.clone());

        let x2 = dods::encode_array(v, Some(counts.iter().product::<usize>()));
        pin_mut!(x2);

        let s: Vec<u8> = futures::executor::block_on_stream(x2)
            .flatten()
            .flatten()
            .collect();

        assert_eq!(x, s);
    }

    #[test]
    fn test_async_read_start_offset() {
        use futures::pin_mut;
        let f = Arc::new(netcdf::open("data/coads_climatology.nc").unwrap());

        let counts = vec![10usize, 30, 80];

        let dir = {
            let v = f.variable("SST").unwrap();

            println!("{}", v.vartype() == netcdf_sys::NC_FLOAT);

            let mut vbuf: Vec<f32> = vec![0.0; counts.iter().product()];
            v.values_to(&mut vbuf, Some(&[1, 10, 10]), Some(&counts))
                .expect("could not read values");

            vbuf
        };

        let v = stream_variable::<f32>(f, "SST".to_string(), vec![1, 10, 10], counts.clone());
        pin_mut!(v);

        let s: Vec<f32> = futures::executor::block_on_stream(v)
            .flatten()
            .flatten()
            .collect();

        assert_eq!(dir, s);
    }

    #[test]
    fn test_async_read_start_zero() {
        use futures::pin_mut;
        let f = Arc::new(netcdf::open("data/coads_climatology.nc").unwrap());

        let counts = vec![10usize, 30, 80];

        let dir = {
            let v = f.variable("SST").unwrap();

            let mut vbuf: Vec<f32> = vec![0.0; counts.iter().product()];
            v.values_to(&mut vbuf, Some(&[0, 0, 0]), Some(&counts))
                .expect("could not read values");

            vbuf
        };

        let v = stream_variable::<f32>(f, "SST".to_string(), vec![0, 0, 0], counts.clone());
        pin_mut!(v);

        let s: Vec<f32> = futures::executor::block_on_stream(v)
            .flatten()
            .flatten()
            .collect();
        assert_eq!(dir, s);
    }

    #[test]
    fn test_async_read_all() {
        use futures::pin_mut;
        let f = Arc::new(netcdf::open("data/coads_climatology.nc").unwrap());

        let counts: Vec<usize> = f
            .variable("SST")
            .unwrap()
            .dimensions()
            .iter()
            .map(|d| d.len())
            .collect();

        let dir = {
            let v = f.variable("SST").unwrap();

            let mut vbuf: Vec<f32> = vec![0.0; counts.iter().product()];
            v.values_to(&mut vbuf, Some(&[0, 0, 0]), Some(&counts))
                .expect("could not read values");

            vbuf
        };

        let v = stream_variable::<f32>(f, "SST".to_string(), vec![0, 0, 0], counts.clone());
        pin_mut!(v);

        let s: Vec<f32> = futures::executor::block_on_stream(v)
            .flatten()
            .flatten()
            .collect();

        assert_eq!(dir, s);
    }
}
