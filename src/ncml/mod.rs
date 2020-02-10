use async_trait::async_trait;
use hyper::{Body, Response, StatusCode};
use notify::{RecommendedWatcher, Watcher};
use same_file::is_same_file;
use std::path::PathBuf;
use walkdir::WalkDir;

use super::dap2::{dds::Dds, dods::StreamingDataset};
use super::nc;
use super::{datasets::FileEvent, Dataset};

mod dds;
mod dods;
mod member;

use member::NcmlMember;

pub enum AggregationType {
    JoinExisting,
}

/// # NCML aggregated datasets
///
/// Reference: https://www.unidata.ucar.edu/software/netcdf-java/current/ncml/Aggregation.html
///
/// ## JoinExisting
///
/// The aggregating dimension must already have a coordinate variable. Only the outer (slowest varying) dimension
/// (first index) may be joined.
///
/// No handling of overlapping coordinate variable is done, it is concatenated in order listed.
pub struct NcmlDataset {
    filename: PathBuf,
    _aggregation_type: AggregationType,
    aggregation_dim: String,
    members: Vec<NcmlMember>,
    das: nc::das::NcDas,
    dds: dds::NcmlDds,
    dim_n: usize,
    _watchers: Vec<RecommendedWatcher>,
}

impl NcmlDataset {
    pub fn open<P>(filename: P, watch: bool) -> anyhow::Result<NcmlDataset>
    where
        P: Into<PathBuf>,
    {
        let filename = filename.into();
        info!("Loading {:?}..", filename);

        let base = filename.parent();

        let xml = std::fs::read_to_string(filename.clone())?;
        let xml = roxmltree::Document::parse(&xml)?;
        let root = xml.root_element();

        let aggregation = root
            .first_element_child()
            .ok_or_else(|| anyhow!("no aggregation tag found"))?;
        ensure!(
            aggregation.tag_name().name() == "aggregation",
            "expected aggregation tag"
        );

        // TODO: use match to enum
        let aggregation_type = aggregation
            .attribute("type")
            .ok_or_else(|| anyhow!("aggregation type not specified"))?;
        ensure!(
            aggregation_type == "joinExisting",
            "only 'joinExisting' type aggregation supported"
        );

        // TODO: only available on certain aggregation types
        let aggregation_dim = aggregation
            .attribute("dimName")
            .ok_or_else(|| anyhow!("aggregation dimension not specified"))?;

        let mut watchers = Vec::new();

        let mut files: Vec<Vec<PathBuf>> = aggregation
            .children()
            .filter(|c| c.is_element())
            .map(|e| match e.tag_name().name() {
                "netcdf" => e.attribute("location").map(|l| {
                    let l = PathBuf::from(l);
                    if l.is_relative() {
                        vec![base.map_or(l.clone(), |b| b.join(l))]
                    } else {
                        vec![l]
                    }
                }),
                "scan" => e.attribute("location").map(|l| {
                    let l: PathBuf = match PathBuf::from(l) {
                        l if l.is_relative() => base.map_or(l.clone(), |b| b.join(l)),
                        l => l,
                    };

                    if let Some(sf) = e.attribute("suffix") {
                        let ignore = e.attribute("ignore");
                        debug!("Scanning {:?}, ignore: {:?}, suffix: {}", l, ignore, sf);

                        if watch {
                            let mf = filename.clone();
                            let ml = l.clone();
                            let mut watcher: RecommendedWatcher = Watcher::new_immediate(move |res: Result<notify::Event, _>|
                                match res {
                                    Ok(event) => {
                                        debug!("Refreshing dataset: {:?}: {:?}", mf, event.paths);

                                        use super::DATA;

                                        let mut data = futures::executor::block_on(DATA.write());
                                        let key = data.make_key(&mf);

                                        if let Some(ds) = data.datasets.get_mut(&key) {
                                            ds.changed(FileEvent::ScanMember(ml.clone(), event)).expect("could not refresh ncml scan tag, should remove");
                                        } else {
                                            error!("could not find dataset.");
                                        }
                                    },
                                    Err(event) => println!("watch error: {:?}", event),
                            }).expect("could not create watcher");

                            watcher
                                .watch(l.clone(), notify::RecursiveMode::NonRecursive)
                                .expect("could not watch ncml root");
                            watchers.push(watcher);
                        }

                        let mut v = Vec::new();

                        for entry in
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
                        {
                            if let Ok(entry) = entry {
                                match entry.metadata() {
                                    Ok(m)
                                        if m.is_file()
                                            && entry
                                                .path()
                                                .to_str()
                                                .map(|s| s.ends_with(sf) && !ignore.map(|i| s.contains(i)).unwrap_or(false)
                                                    )
                                                .unwrap_or(false)
                                                =>
                                    {
                                        v.push(std::fs::canonicalize(entry.into_path()).ok())
                                    }
                                    _ => (),
                                }
                            };
                        }
                        v.sort();
                        v.into_iter().collect::<Option<Vec<PathBuf>>>()
                    } else {
                        error!("no suffix specified in ncml scan tag");
                        None
                    }
                }).flatten(),
                t => {
                    error!("unknown tag: {}", t);
                    None
                }
            })
            .collect::<Option<Vec<Vec<PathBuf>>>>()
            .ok_or_else(|| anyhow!("could not parse file list"))?;
        files.sort();

        let mut members = files
            .iter()
            .flatten()
            .map(|p| NcmlMember::open(p, aggregation_dim, watch))
            .collect::<Result<Vec<NcmlMember>, _>>()?;

        members.sort_by(|a, b| {
            a.rank
                .partial_cmp(&b.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // DAS should be same for all members (hopefully), using first.
        let first = members
            .first()
            .ok_or_else(|| anyhow!("no members in aggregate"))?;
        let das = nc::das::NcDas::build(&first.f)?;

        let dim_n: usize = members.iter().map(|m| m.n).sum();
        let dds = dds::NcmlDds::build(&first.f, &filename, aggregation_dim, dim_n)?;

        Ok(NcmlDataset {
            filename: filename.clone(),
            _aggregation_type: AggregationType::JoinExisting,
            aggregation_dim: aggregation_dim.to_string(),
            members,
            das,
            dds,
            dim_n,
            _watchers: watchers,
        })
    }
}

#[async_trait]
impl Dataset for NcmlDataset {
    fn name(&self) -> String {
        self.filename.to_string_lossy().to_string()
    }

    async fn das(&self) -> Result<Response<Body>, hyper::http::Error> {
        Response::builder()
            .header("Content-Type", "text/plain")
            .header("Content-Description", "dods-das")
            .header("XDODS-Server", "dars")
            .body(Body::from(self.das.to_string()))
    }

    async fn dds(&self, query: Option<&str>) -> Result<Response<Body>, hyper::http::Error> {
        match self.dds.parse_query(query) {
            Ok(query) => match self.dds.dds(&self.members[0].f, &query) {
                Ok(dds) => Response::builder()
                    .header("Content-Type", "text/plain")
                    .header("Content-Description", "dods-dds")
                    .header("XDODS-Server", "dars")
                    .body(Body::from(dds)),
                _ => Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::empty()),
            },
            Err(_) => Response::builder()
                .status(StatusCode::UNPROCESSABLE_ENTITY)
                .body(Body::empty()),
        }
    }

    async fn dods(&self, query: Option<&str>) -> Result<Response<Body>, hyper::http::Error> {
        use futures::stream::{self, Stream, StreamExt};
        use std::pin::Pin;

        let query = if let Ok(query) = self.dds.parse_query(query) {
            query
        } else {
            return Response::builder()
                .status(StatusCode::UNPROCESSABLE_ENTITY)
                .body(Body::empty());
        };

        let dds = if let Ok(r) = self.dds.dds(&self.members[0].f.clone(), &query) {
            r.into_bytes()
        } else {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty());
        };

        let dods: Vec<
            Pin<Box<dyn Stream<Item = Result<Vec<u8>, anyhow::Error>> + Send + Sync + 'static>>,
        > = query
            .iter()
            .map(|(v, i, c)| {
                self.stream_encoded_variable(
                    &v,
                    i.as_ref().map(|i| i.as_slice()),
                    c.as_ref().map(|c| c.as_slice()),
                )
            })
            .collect();

        let s = stream::once(async move { Ok::<_, anyhow::Error>(dds) })
            .chain(stream::once(async {
                Ok::<_, anyhow::Error>(String::from("\nData:\r\n").into_bytes())
            }))
            .chain(stream::iter(dods).flatten())
            .inspect(|e| {
                if let Err(e) = e {
                    error!("error while streaming: {:?}", e);
                }
            });

        Response::builder().body(Body::wrap_stream(s))
    }

    async fn raw(&self) -> Result<Response<Body>, hyper::http::Error> {
        Response::builder()
            .status(StatusCode::UPGRADE_REQUIRED)
            .header("Upgrade", "DAP/2")
            .body(Body::from("Try using a DAP client."))
    }

    fn changed(&mut self, e: FileEvent) -> Result<(), anyhow::Error> {
        // we only get called for scan tags
        use notify::event::{CreateKind, EventKind::*, RemoveKind};

        let FileEvent::ScanMember(_, e) = e;

        let mut changed = false;

        match e.kind {
            Create(ck) => match ck {
                CreateKind::File => {
                    for p in e.paths {
                        if self
                            .members
                            .iter()
                            .find(|m| is_same_file(&p, &m.filename).unwrap_or(false))
                            .is_none()
                        {
                            warn!("{:?}: adding member: {:?}", self.filename, p);
                            if let Ok(m) =
                                NcmlMember::open(p.clone(), self.aggregation_dim.clone(), true)
                            {
                                self.members.push(m);
                                changed = true;
                            } else {
                                error!("{:?}: could not add member: {:?}", self.filename, p);
                            }
                        }
                    }
                }
                CreateKind::Folder => unimplemented!("scan dir"),
                _ => (),
            },
            Remove(rk) => match rk {
                RemoveKind::File => {
                    for p in e.paths {
                        if let Some((i, _)) = self
                            .members
                            .iter()
                            .enumerate()
                            .find(|(_, m)| p == m.filename)
                        {
                            warn!("{:?}: removing member: {:?}", self.filename, p);
                            self.members.remove(i);
                            changed = true;
                        } else {
                            error!("{:?} not a member", p);
                        }
                    }
                }
                RemoveKind::Folder => unimplemented!("scan dir"),
                _ => (),
            },
            _ => (), //warn!("{:?}: event not handled: {:?}", self.filename, e)
        };

        if changed {
            self.members.sort_by(|a, b| {
                a.rank
                    .partial_cmp(&b.rank)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            let first = self
                .members
                .first()
                .ok_or_else(|| anyhow!("no members in aggregate"))?;
            self.das = nc::das::NcDas::build(&first.f)?;

            let dim_n: usize = self.members.iter().map(|m| m.n).sum();
            self.dds = dds::NcmlDds::build(&first.f, &self.filename, &self.aggregation_dim, dim_n)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ncml_open() {
        crate::testcommon::init();
        let nm = NcmlDataset::open("data/ncml/aggExisting.ncml", true).unwrap();

        println!("files: {:#?}", nm.members);
    }
}
