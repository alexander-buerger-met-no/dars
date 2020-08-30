use std::fmt;
use std::path::{Path, PathBuf};

use async_stream::stream;
use bytes::Bytes;
use futures::{pin_mut, Stream, StreamExt};

use dap2::dds::DdsVariableDetails;
use dap2::dods::xdr_length;
use hidefix::idx;

mod das;
pub(crate) mod dds;

/// HDF5 dataset source.
pub struct Hdf5Dataset {
    path: PathBuf,
    idxkey: String,
    das: dap2::Das,
    dds: dap2::Dds,
    modified: std::time::SystemTime,
}

impl fmt::Debug for Hdf5Dataset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hdf5Dataset <{:?}>", self.path)
    }
}

pub struct HDF5File(pub hdf5::File, pub String);

impl Hdf5Dataset {
    pub fn open<P: AsRef<Path>>(path: P, key: String, db: &sled::Db) -> anyhow::Result<Hdf5Dataset> {
        let path = path.as_ref();

        let modified = std::fs::metadata(&path)?.modified()?;

        let _silence = hdf5::silence_errors();
        let hf = HDF5File(hdf5::File::open(&path)?, key.clone());

        trace!("Building DAS of {:?}..", path);
        let das = (&hf).into();

        trace!("Building DDS of {:?}..", path);
        let dds = (&hf).into();

        let idxkey = std::fs::canonicalize(path)?.to_string_lossy().to_string();
        if !db.contains_key(&idxkey)? {
            debug!("Indexing: {:?}..", path);
            let idx = hdf5::sync::sync(||
                idx::Index::index_file(&hf.0, Some(&path)))?;
            let bts = bincode::serialize(&idx)?;

            trace!("Inserting index into db ({})", idxkey);
            db.insert(&idxkey, bts)?;
        } else {
            trace!("{} already indexed.", idxkey);
        };

        Ok(Hdf5Dataset {
            path: path.into(),
            idxkey,
            das,
            dds,
            modified,
        })
    }

    pub async fn raw(
        &self,
    ) -> Result<
        (
            u64,
            impl Stream<Item = Result<hyper::body::Bytes, std::io::Error>>,
        ),
        std::io::Error,
    > {
        use tokio::fs::File;
        use tokio_util::codec;
        use tokio_util::codec::BytesCodec;

        let sz = std::fs::metadata(&self.path)?.len();

        File::open(&self.path).await.map(|file| {
            (
                sz,
                codec::FramedRead::new(file, BytesCodec::new())
                    .map(|r| r.map(|bytes| bytes.freeze())),
            )
        })
    }

    pub async fn das(&self) -> &dap2::Das {
        &self.das
    }

    pub async fn dds(&self) -> &dap2::Dds {
        &self.dds
    }

    pub async fn variable(
        &self,
        variable: &DdsVariableDetails,
        db: sled::Db
    ) -> Result<impl Stream<Item = Result<Bytes, anyhow::Error>> + Send + 'static, anyhow::Error>
    {
        let modified = std::fs::metadata(&self.path)?.modified()?;
        if modified != self.modified {
            warn!("{:?} has changed on disk", self.path);
            return Err(anyhow!("{:?} has changed on disk", self.path));
        }

        debug!(
            "streaming: {} [{:?} / {:?}]",
            variable.name, variable.indices, variable.counts
        );

        trace!("fetching index from db: {}", self.idxkey);
        let bts = db.get(&self.idxkey)?.unwrap();
        let idx = bincode::deserialize::<idx::Index>(&bts)?;

        trace!("creating streamer: {}", variable.name);

        let reader = match idx.dataset(&variable.name) {
            Some(ds) => ds.as_streamer(&self.path),
            None => Err(anyhow!("dataset does not exist")),
        }?;

        let indices: Vec<u64> = variable.indices.iter().map(|c| *c as u64).collect();
        let counts: Vec<u64> = variable.counts.iter().map(|c| *c as u64).collect();

        let length = if !variable.is_scalar() {
            Some(Bytes::from(Vec::from(xdr_length(variable.len() as u32))))
        } else {
            None
        };

        let bytes = reader.stream(Some(indices.as_slice()), Some(counts.as_slice()));

        Ok(stream! {
            if let Some(length) = length {
                yield Ok(length);
            }

            pin_mut!(bytes);

            while let Some(b) = bytes.next().await {
                yield b;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dap2::constraint::Constraint;
    use dap2::dds::ConstrainedVariable;
    use futures::executor::{block_on, block_on_stream};
    use futures::pin_mut;
    use test::Bencher;
    use crate::data::test_db;

    #[test]
    fn open_coads() {
        let db = test_db();
        Hdf5Dataset::open("../data/coads_climatology.nc4", "coads".into(), &db).unwrap();
    }

    #[bench]
    fn coads_stream_sst_struct(b: &mut Bencher) {
        let db = test_db();
        let hd = Hdf5Dataset::open("../data/coads_climatology.nc4", "coads".into(), &db).unwrap();

        let c = Constraint::parse("SST.SST").unwrap();
        let dds = hd.dds.dds(&c).unwrap();

        assert_eq!(dds.variables.len(), 1);
        if let ConstrainedVariable::Structure {
            variable: _,
            member,
        } = &dds.variables[0]
        {
            b.iter(|| {
                let db = db.clone();
                let reader = block_on(hd.variable(&member, db)).unwrap();
                pin_mut!(reader);
                block_on_stream(reader).for_each(drop);
            });
        } else {
            panic!("wrong constrained variable");
        }
    }

    #[bench]
    fn coads_get_modified_time(b: &mut Bencher) {
        b.iter(|| {
            let m = std::fs::metadata("../data/coads_climatology.nc4").unwrap();
            test::black_box(m.modified().unwrap());
        })
    }
}
