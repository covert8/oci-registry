use core::time::Duration;
use std::iter;
use std::sync::Arc;

use actix_web::http;
use actix_web::rt;
use actix_web::web;
use actix_web::HttpResponse;
use arcerror::ArcError;
use arcstr::ArcStr;
use dkregistry::v2::Client;
use futures::stream;
use futures::StreamExt;
use futures::TryStreamExt;
use serde::Deserialize;
use tracing::error;
use tracing::warn;

use crate::image::ImageName;
use crate::image::ImageReference;
use crate::storage::Manifest;
use crate::storage::Repository;
use crate::InvalidationTime;

mod error;
use error::Error;

async fn authenticate_with_upstream(upstream: &mut Client, scope: &str) -> Result<(), dkregistry::errors::Error> {
	upstream.authenticate(&[scope]).await?;
	Ok(())
}

pub async fn root(upstream: web::Data<Client>) -> Result<&'static str, Error> {
	Arc::make_mut(&mut upstream.into_inner())
		.clone()
		.authenticate(&[])
		.await?;
	Ok("")
}

#[derive(Debug, Deserialize)]
pub struct ManifestRequest {
	image: ImageName,
	reference: ImageReference
}

impl ManifestRequest {
	fn path(&self) -> String {
		format!("/{}/manifests/{}", self.image, self.reference)
	}
}

async fn get_manifest(req: &ManifestRequest, max_age: Duration, repo: &Repository, upstream: web::Data<Client>) -> Result<Manifest, Error> {
	let path = req.path();
	let path = path.strip_prefix("/").unwrap();
	match repo.clone().read(&path, max_age).await {
		Ok(stream) => {
			let body = stream.try_collect::<web::BytesMut>().await?;
			let manifest = serde_json::from_slice(body.as_ref())?;
			return Ok(manifest);
		},
		Err(e) => warn!("{} not found in repository ({}); pulling from upstream", path, e)
	}

	let mut upstream = (*upstream.into_inner()).clone();
	authenticate_with_upstream(&mut upstream, &format!("repository:{}:pull", req.image.as_ref())).await?;
	let (manifest, media_type, digest) = upstream.get_raw_manifest_and_metadata(req.image.as_ref(), &req.reference.to_string()).await?;
	let manifest = Manifest::new(manifest, media_type, digest);

	let body = serde_json::to_vec(&manifest).unwrap();
	let len = body.len().try_into().unwrap_or(i64::MAX);
	if let Err(e) = repo.write(&path, stream::iter(iter::once(Result::<_, std::io::Error>::Ok(body.into()))), len).await {
		error!("{}", e);
	}
	Ok(manifest)
}

pub async fn manifest(path: web::Path<ManifestRequest>, invalidation: web::Data<InvalidationTime>, repo: web::Data<Repository>, upstream: web::Data<Client>) -> Result<HttpResponse, Error> {
	let manifest = get_manifest(path.as_ref(), invalidation.manifest, repo.as_ref(), upstream).await?;

	let mut response = HttpResponse::Ok();
	response.insert_header((http::header::CONTENT_TYPE, manifest.media_type.to_string()));
	if let Some(digest) = manifest.digest.clone() {
		response.insert_header(("Docker-Content-Digest", digest));
	}
	Ok(response.body(manifest.manifest))
}

#[derive(Debug, Deserialize)]
pub struct BlobRequest {
	image: ImageName,
	digest: String
}

impl BlobRequest {
	fn path(&self) -> ArcStr {
		format!("/{}/blobs/{}", self.image, self.digest).into()
	}
}

pub async fn blob(path: web::Path<BlobRequest>, invalidation: web::Data<InvalidationTime>, repo: web::Data<Repository>, upstream: web::Data<Client>) -> Result<HttpResponse, Error> {
	if(!path.digest.starts_with("sha256:")) {
		return Err(Error::InvalidDigest);
	}

	let req_path = path.path();
	let storage_path = req_path.strip_prefix("/").unwrap();
	match (*repo.clone().into_inner()).clone().read(storage_path, invalidation.blob).await {
		Ok(stream) => return Ok(HttpResponse::Ok().streaming(stream)),
		Err(e) => warn!("{} not found in repository ({}); pulling from upstream", storage_path, e)
	};

	let mut upstream = (*upstream.into_inner()).clone();
	authenticate_with_upstream(&mut upstream, &format!("repository:{}:pull", path.image.as_ref())).await?;
	let response = upstream.get_blob_response(path.image.as_ref(), path.digest.as_ref()).await?;

	let len = response.size().unwrap_or_default();
	let (tx, rx) = async_broadcast::broadcast(16);
	{
		let req_path = req_path.clone();
		let mut stream = response.stream();
		rt::spawn(async move {
			while let Some(chunk) = stream.next().await {
				let chunk = match chunk {
					Ok(v) => Ok(v),
					Err(e) => {
						error!("Error reading from upstream:  {}", e);
						Err(ArcError::from(e))
					}
				};
				if let Err(_) = tx.broadcast(chunk).await {
					error!("Readers for proxied blob request {} all closed", req_path);
					break;
				}
			}
		});
	}

	let rx2 = rx.clone();
	rt::spawn(async move {
		if let Err(e) = repo.write(req_path.strip_prefix("/").unwrap(), rx2, len.try_into().unwrap_or(i64::MAX)).await {
			error!("{}", e);
		}
	});

	Ok(HttpResponse::Ok().streaming(rx))
}

