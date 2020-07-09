use std::cmp::min;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_stream::stream;
use bytes::{Bytes, BytesMut};
use futures::{executor::block_on_stream, pin_mut, Stream, StreamExt};
use roxmltree::Node;
use walkdir::WalkDir;

use crate::hdf5::HDF5File;
use dap2::dds::DdsVariableDetails;
use dap2::dods::xdr_length;
use hidefix::reader::stream;

mod dds;
mod member;
use member::NcmlMember;

/// # NCML aggregated datasets
///
/// Reference: https://www.unidata.ucar.edu/software/netcdf-java/current/ncml/Aggregation.html
///
/// ## JoinExisting
///
/// The aggregating dimension must already have a coordinate variable. Only the slowest varying or outer dimension
/// (first index) may be joined.
///
/// No handling of overlapping coordinate variable is done, it is concatenated in order listed.
pub struct NcmlDataset {
    path: PathBuf,
    das: dap2::Das,
    dds: dap2::Dds,
    /// Aggregation dimension
    dimension: String,
    coordinates: CoordinateVariable,
    modified: std::time::SystemTime,
    members: Arc<Vec<NcmlMember>>,
}

impl fmt::Debug for NcmlDataset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NcmlDataset <{:?}>", self.path)
    }
}

impl NcmlDataset {
    pub fn open<P>(path: P) -> anyhow::Result<NcmlDataset>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        let modified = std::fs::metadata(path)?.modified()?;
        info!("Loading {:?}..", path);

        // Parse NCML file.
        let xml = std::fs::read_to_string(&path)?;
        let xml = roxmltree::Document::parse(&xml)?;
        let root = xml.root_element();

        let aggregation = root
            .first_element_child()
            .ok_or_else(|| anyhow!("no aggregation tag found"))?;
        ensure!(
            aggregation.tag_name().name() == "aggregation",
            "expected aggregation tag"
        );

        let aggregation_type = aggregation
            .attribute("type")
            .ok_or_else(|| anyhow!("aggregation type not specified"))?;
        ensure!(
            aggregation_type == "joinExisting",
            "only 'joinExisting' type aggregation supported"
        );

        // TODO: only available on certain aggregation types
        let dimension = aggregation
            .attribute("dimName")
            .ok_or_else(|| anyhow!("aggregation dimension not specified"))?
            .to_string();
        trace!("Coordinate variable: {}", dimension);

        let files = NcmlDataset::get_member_files(path.parent(), &aggregation)?;

        let mut members = files
            .iter()
            .map(|p| NcmlMember::open(p, &dimension))
            .collect::<Result<Vec<NcmlMember>, _>>()?;

        members.sort_by(|a, b| {
            a.rank
                .partial_cmp(&b.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        ensure!(members.len() > 0, "no members in aggregate.");
        let members = Arc::new(members);

        trace!("Building DAS..");
        let das = {
            // DAS should be the same regardless of files, using first member.
            let path = &members[0].path;
            let hf = HDF5File(hdf5::File::open(path)?, path.to_path_buf());
            (&hf).into()
        };

        trace!("Building DDS..");
        let dds = {
            let ipath = &members[0].path;
            let n = members.iter().map(|m| m.n).sum();
            dds::NcmlDdsBuilder::new(hdf5::File::open(ipath)?, path.into(), dimension.clone(), n)
                .into()
        };

        debug!("Reading coordinate variable..");
        let coordinates = CoordinateVariable::from(&members, &dimension)?;

        Ok(NcmlDataset {
            path: path.into(),
            das,
            dds,
            dimension,
            coordinates,
            modified,
            members,
        })
    }

    fn get_member_files(base: Option<&Path>, aggregation: &Node) -> anyhow::Result<Vec<PathBuf>> {
        aggregation
            .children()
            .filter(|c| c.is_element())
            .filter_map(|e| match e.tag_name().name() {
                "netcdf" => e.attribute("location").map(|l| {
                    let l = PathBuf::from(l);
                    if l.is_relative() {
                        Ok(vec![base.map_or(l.clone(), |b| b.join(l))])
                    } else {
                        Ok(vec![l])
                    }
                }),
                "scan" => e.attribute("location").map(|l| {
                    let l: PathBuf = match PathBuf::from(l) {
                        l if l.is_relative() => base.map_or(l.clone(), |b| b.join(l)),
                        l => l,
                    };

                    if let Some(sf) = e.attribute("suffix") {
                        let ignore = e.attribute("ignore");
                        trace!("Scanning {:?}, ignore: {:?}, suffix: {}", l, ignore, sf);
                        WalkDir::new(l)
                            .follow_links(true)
                            .into_iter()
                            .filter_entry(|entry| {
                                !entry
                                    .file_name()
                                    .to_str()
                                    .map(|s| s.starts_with('.'))
                                    .unwrap_or(false)
                            })
                            .filter_map(|entry| {
                                entry
                                    .ok()
                                    .map(|entry| {
                                        entry
                                            .metadata()
                                            .ok()
                                            .map(|m| {
                                                if m.is_file()
                                                    && entry
                                                        .path()
                                                        .to_str()
                                                        .map(|s| {
                                                            s.ends_with(sf)
                                                                && !ignore
                                                                    .map(|i| s.contains(i))
                                                                    .unwrap_or(false)
                                                        })
                                                        .unwrap_or(false)
                                                {
                                                    Some(entry.into_path())
                                                } else {
                                                    None
                                                }
                                            })
                                            .flatten()
                                    })
                                    .flatten()
                            })
                            .map(|path| {
                                std::fs::canonicalize(path)
                                    .map_err(|e| anyhow!("failed to scan member: {:?}", e))
                            })
                            .collect::<Result<Vec<_>, _>>()
                    } else {
                        Err(anyhow!("no suffix specified in ncml scan tag"))
                    }
                }),
                t => {
                    error!("unknown tag: {}", t);
                    None
                }
            })
            .collect::<Result<Vec<Vec<_>>, _>>()
            .map(|vecs| vecs.into_iter().flatten().collect())
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

        let indices: Vec<u64> = variable.indices.iter().map(|c| *c as u64).collect();
        let counts: Vec<u64> = variable.counts.iter().map(|c| *c as u64).collect();

        let length = if !variable.is_scalar() {
            Some(Bytes::from(Vec::from(xdr_length(variable.len() as u32))))
        } else {
            None
        };

        enum ABC<A, B, C> {
            A(A),
            B(B),
            C(C),
        };

        let bytes = if variable.name == self.dimension {
            // Coordinate dimension (aggregation variable).
            ABC::A(
                self.coordinates
                    .stream(indices.as_slice(), counts.as_slice())?,
            )
        } else if variable
            .dimensions
            .get(0)
            .map(|d| d.0 != self.dimension)
            .unwrap_or(true)
        {
            // Non-aggregated variable, using first member.
            ABC::B(
                self.members[0]
                    .stream(&variable.name, indices.as_slice(), counts.as_slice())
                    .await?,
            )
        } else {
            // Aggregated variable
            let members = Arc::clone(&self.members);
            let var = variable.name.clone();

            ABC::C(stream! {
                trace!("streaming aggregated variable");
                let mut member_start = 0;

                for m in &*members  {
                    if indices[0] >= member_start && indices[0] < (member_start + m.n as u64) {
                        let mut mindices = indices.clone();
                        mindices[0] = indices[0] - member_start;

                        let mut mcounts = counts.clone();
                        mcounts[0] = min(counts[0], m.n as u64 - mindices[0]);

                        trace!("First file: {} to {} (mi = {:?}, mc = {:?})", member_start, member_start + m.n as u64, mindices, mcounts);

                        let bytes = m.stream(&var, &mindices, &mcounts).await?;
                        pin_mut!(bytes);
                        while let Some(b) = bytes.next().await {
                            yield b;
                        }
                    } else if indices[0] < member_start && (member_start < indices[0] + counts[0]) {
                        let mut mcounts = counts.clone();
                        mcounts[0] = min(indices[0] + counts[0] - member_start, m.n as u64);

                        let mut mindices = indices.clone();
                        mindices[0] = 0;

                        trace!(
                            "Consecutive file at {} to {} (i = {:?}, c = {:?})",
                            member_start,
                            member_start + m.n as u64,
                            mindices,
                            mcounts
                        );

                        let bytes = m.stream(&var, &mindices, &mcounts).await?;
                        pin_mut!(bytes);
                        while let Some(b) = bytes.next().await {
                            yield b;
                        }
                    } else if indices[0] + counts[0] < member_start {
                        break;
                    }

                    member_start += m.n as u64;
                }
            })
        };

        Ok(stream! {
            if let Some(length) = length {
                yield Ok(length);
            }

            match bytes {
                ABC::A(bytes) => {
                    pin_mut!(bytes);
                    while let Some(b) = bytes.next().await {
                        yield b;
                    }
                },
                ABC::B(bytes) => {
                    pin_mut!(bytes);
                    while let Some(b) = bytes.next().await {
                        yield b;
                    }
                },
                ABC::C(bytes) => {
                    pin_mut!(bytes);
                    while let Some(b) = bytes.next().await {
                        yield b;
                    }
                },
            }
        })
    }
}

/// The coordinate variable is cached since it is always requested and requires all files to be
/// opened and read.
pub struct CoordinateVariable {
    bytes: Bytes,
    /// Data type size
    dsz: usize,
}

impl CoordinateVariable {
    pub fn from(members: &Vec<NcmlMember>, dimension: &str) -> anyhow::Result<CoordinateVariable> {
        ensure!(!members.is_empty(), "no members");

        let dsz = members[0]
            .idx
            .dataset(dimension)
            .ok_or_else(|| anyhow!("dimension dataset not found."))?
            .dsize;
        let n: usize = members.iter().map(|m| m.n).sum();

        let mut bytes = BytesMut::with_capacity(n * dsz);

        for m in members {
            let ds = m
                .idx
                .dataset(dimension)
                .ok_or_else(|| anyhow!("dimension dataset not found."))?;
            let reader = stream::DatasetReader::with_dataset(&ds, &m.path)?;
            let reader = reader.stream(None, None);

            pin_mut!(reader);

            block_on_stream(reader)
                .try_for_each(|b| b.map(|b| bytes.extend_from_slice(b.as_ref())))?;
        }

        trace!(
            "Coordinate variable: {}, length: {}, data type size: {}",
            dimension,
            bytes.len(),
            dsz
        );

        Ok(CoordinateVariable {
            bytes: bytes.freeze(),
            dsz,
        })
    }

    pub fn stream(
        &self,
        indices: &[u64],
        counts: &[u64],
    ) -> Result<impl Stream<Item = Result<Bytes, anyhow::Error>> + Send + 'static, anyhow::Error>
    {
        ensure!(
            indices.len() == 1 && counts.len() == 1,
            "coordinate dimension is always 1 dimension"
        );
        let start = indices[0] as usize * self.dsz;
        let end = (indices[0] + counts[0]) as usize * self.dsz;
        ensure!(end <= self.bytes.len(), "slab out of range");

        let bytes = self.bytes.slice(start..end);
        Ok(futures::stream::once(async { Ok(bytes) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn agg_existing_location() {
        let _ = env_logger::builder().is_test(true).try_init();
        let ncml = NcmlDataset::open("../data/ncml/aggExisting.ncml").unwrap();

        assert_eq!(ncml.coordinates.bytes.len(), 4 * (31 + 28));
    }

    #[tokio::test]
    async fn agg_existing_scan() {
        let _ = env_logger::builder().is_test(true).try_init();
        let ncml = NcmlDataset::open("../data/ncml/scan.ncml").unwrap();

        assert_eq!(ncml.coordinates.bytes.len(), 4 * (31 + 28));
    }
}
