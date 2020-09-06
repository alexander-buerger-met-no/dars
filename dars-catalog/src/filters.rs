use std::convert::Infallible;
use std::sync::Arc;
use tera::Tera;
use warp::Filter;

use crate::handlers;
use crate::Catalog;

pub fn catalog<T: Catalog + Clone>(
    root: String,
    tera: Arc<Tera>,
    catalog: T,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    folder(root, tera, catalog)
}

fn folder<T: Catalog + Clone>(
    root: String,
    tera: Arc<Tera>,
    catalog: T,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    warp::path("data")
        .and(warp::get())
        .and(with_root(root))
        .map(move |r| (r, Arc::clone(&tera)))
        .untuple_one()
        .and(elements(catalog))
        .and_then(handlers::folder)
}

/// Return the elements of the folder, if the path represents a folder.
fn elements<T: Catalog + Clone>(
    catalog: T,
) -> impl Filter<Extract = (String, (Vec<String>, Vec<String>)), Error = warp::reject::Rejection> + Clone {
    warp::path::peek()
        .map(|p: warp::filters::path::Peek| p.as_str().to_string())
        .and(
            warp::path::tail().and_then(move |tail: warp::filters::path::Tail| {
                let catalog = catalog.clone();
                async move {
                    let mut tail = tail.as_str().to_string();
                    if tail.len() > 0 && !tail.ends_with('/') {
                        tail.push('/');
                    }

                    let (folders, mut paths) = catalog
                        .paths()
                        .filter_map(|p| {
                            if p.starts_with(&tail) {
                                Some(String::from(p))
                            } else {
                                None
                            }
                        })
                        .partition::<Vec<String>, _>(|p| p[tail.len()..].contains('/'));

                    paths.sort();

                    // remove trailing names + make unique
                    let mut folders = folders.iter().map(|p|
                        p[..p.find('/').unwrap()].to_string()).collect::<Vec<String>>();
                    folders.sort();
                    folders.dedup();

                    if paths.len() == 0 || (paths.len() == 1 && paths[0] == tail) {
                        Err(warp::reject())
                    } else {
                        Ok((folders, paths))
                    }
                }
            }),
        )
}

fn with_root(root: String) -> impl Filter<Extract = (String,), Error = Infallible> + Clone {
    warp::any().map(move || root.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::*;
    use futures::executor::block_on;

    #[test]
    fn elements_subpath() {
        let catalog = TestCatalog::test();

        assert_eq!(
            block_on(
                warp::test::request()
                    .path("/path1/")
                    .filter(&elements(catalog))
            )
            .unwrap().1,
            ["path1/hula.nc", "path1/hula2.nc"]
        );
    }
}
