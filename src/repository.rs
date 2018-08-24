//! Interfaces for interacting with different types of TUF repositories.

use hyper::client::response::Response;
use hyper::header::{Headers, UserAgent};
use hyper::status::StatusCode;
use hyper::{Client, Url};
use std::collections::HashMap;
use std::fs::{DirBuilder, File};
use std::io::{self, Cursor, Read, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tempfile::NamedTempFile;

use crypto::{self, HashAlgorithm, HashValue};
use error::Error;
use interchange::DataInterchange;
use metadata::{
    Metadata, MetadataPath, MetadataVersion, SignedMetadata, TargetDescription, TargetPath,
};
use util::SafeReader;
use Result;

/// Top-level trait that represents a TUF repository and contains all the ways it can be interacted
/// with.
pub trait Repository<D>
where
    D: DataInterchange,
{
    /// The type returned when reading a target.
    type TargetRead: Read;

    /// Store signed metadata.
    ///
    /// Note: This **MUST** canonicalize the bytes before storing them as a read will expect the
    /// hashes of the metadata to match.
    fn store_metadata<M>(
        &self,
        meta_path: &MetadataPath,
        version: &MetadataVersion,
        metadata: &SignedMetadata<D, M>,
    ) -> Result<()>
    where
        M: Metadata;

    /// Fetch signed metadata.
    fn fetch_metadata<M>(
        &self,
        meta_path: &MetadataPath,
        version: &MetadataVersion,
        max_size: &Option<usize>,
        min_bytes_per_second: u32,
        hash_data: Option<(&HashAlgorithm, HashValue)>,
    ) -> Result<SignedMetadata<D, M>>
    where
        M: Metadata;

    /// Store the given target.
    fn store_target<R>(&self, read: R, target_path: &TargetPath) -> Result<()>
    where
        R: Read;

    /// Fetch the given target.
    fn fetch_target(
        &self,
        target_path: &TargetPath,
        target_description: &TargetDescription,
        min_bytes_per_second: u32,
    ) -> Result<SafeReader<Self::TargetRead>>;

    /// Perform a sanity check that `M`, `Role`, and `MetadataPath` all desrcribe the same entity.
    fn check<M>(meta_path: &MetadataPath) -> Result<()>
    where
        M: Metadata,
    {
        if !M::ROLE.fuzzy_matches_path(meta_path) {
            return Err(Error::IllegalArgument(format!(
                "Role {} does not match path {:?}",
                M::ROLE,
                meta_path
            )));
        }

        Ok(())
    }
}

/// A repository contained on the local file system.
pub struct FileSystemRepository<D>
where
    D: DataInterchange,
{
    local_path: PathBuf,
    interchange: PhantomData<D>,
}

impl<D> FileSystemRepository<D>
where
    D: DataInterchange,
{
    /// Create a new repository on the local file system.
    pub fn new(local_path: PathBuf) -> Result<Self> {
        for p in &["metadata", "targets", "temp"] {
            DirBuilder::new()
                .recursive(true)
                .create(local_path.join(p))?
        }

        Ok(FileSystemRepository {
            local_path,
            interchange: PhantomData,
        })
    }
}

impl<D> Repository<D> for FileSystemRepository<D>
where
    D: DataInterchange,
{
    type TargetRead = File;

    fn store_metadata<M>(
        &self,
        meta_path: &MetadataPath,
        version: &MetadataVersion,
        metadata: &SignedMetadata<D, M>,
    ) -> Result<()>
    where
        M: Metadata,
    {
        Self::check::<M>(meta_path)?;

        let mut path = self.local_path.join("metadata");
        path.extend(meta_path.components::<D>(version));

        if path.exists() {
            debug!("Metadata path exists. Overwriting: {:?}", path);
        }

        atomically_write(&path, |write| {
            D::to_writer(write, metadata)?;
            Ok(())
        })
    }

    /// Fetch signed metadata.
    fn fetch_metadata<M>(
        &self,
        meta_path: &MetadataPath,
        version: &MetadataVersion,
        max_size: &Option<usize>,
        min_bytes_per_second: u32,
        hash_data: Option<(&HashAlgorithm, HashValue)>,
    ) -> Result<SignedMetadata<D, M>>
    where
        M: Metadata,
    {
        Self::check::<M>(meta_path)?;

        let mut path = self.local_path.join("metadata");
        path.extend(meta_path.components::<D>(&version));

        let read = SafeReader::new(
            File::open(&path)?,
            max_size.unwrap_or(::std::usize::MAX) as u64,
            min_bytes_per_second,
            hash_data,
        )?;

        Ok(D::from_reader(read)?)
    }

    fn store_target<R>(&self, mut read: R, target_path: &TargetPath) -> Result<()>
    where
        R: Read,
    {
        let mut path = self.local_path.join("targets");
        path.extend(target_path.components());

        if path.exists() {
            debug!("Target path exists. Overwriting: {:?}", path);
        }

        atomically_write(&path, |write| {
            io::copy(&mut read, write)?;
            Ok(())
        })
    }

    fn fetch_target(
        &self,
        target_path: &TargetPath,
        target_description: &TargetDescription,
        min_bytes_per_second: u32,
    ) -> Result<SafeReader<Self::TargetRead>> {
        let mut path = self.local_path.join("targets");
        path.extend(target_path.components());

        if !path.exists() {
            return Err(Error::NotFound);
        }

        let (alg, value) = crypto::hash_preference(target_description.hashes())?;

        SafeReader::new(
            File::open(&path)?,
            target_description.size(),
            min_bytes_per_second,
            Some((alg, value.clone())),
        )
    }
}

fn atomically_write<F>(path: &Path, mut f: F) -> Result<()>
where
    F: FnMut(&mut Write) -> Result<()>,
{
    // We want to atomically write the file to make sure clients can never see a partially written file.
    // In order to do this, we'll write to a temporary file in the same directory as our target, otherwise
    // we risk writing the temporary file to one mountpoint, and then non-atomically copying the file to another mountpoint.

    let mut temp_file = if let Some(parent) = path.parent() {
        DirBuilder::new().recursive(true).create(parent)?;
        NamedTempFile::new_in(parent)?
    } else {
        NamedTempFile::new_in(".")?
    };

    f(&mut temp_file)?;

    temp_file.persist(&path)?;

    Ok(())
}

/// A repository accessible over HTTP.
pub struct HttpRepository<D>
where
    D: DataInterchange,
{
    url: Url,
    client: Client,
    user_agent: String,
    metadata_prefix: Option<Vec<String>>,
    interchange: PhantomData<D>,
}

impl<D> HttpRepository<D>
where
    D: DataInterchange,
{
    /// Create a new repository with the given `Url` and `Client`.
    ///
    /// Callers *should* include a custom User-Agent prefix to help maintainers of TUF repositories
    /// keep track of which client versions exist in the field.
    ///
    /// The argument `metadata_prefix` is used provide an alternate path where metadata is stored on
    /// the repository. If `None`, this defaults to `/`. For example, if there is a TUF repository
    /// at `https://tuf.example.com/`, but all metadata is stored at `/meta/`, then passing the
    /// arg `Some("meta".into())` would cause `root.json` to be fetched from
    /// `https://tuf.example.com/meta/root.json`.
    pub fn new(
        url: Url,
        client: Client,
        user_agent_prefix: Option<String>,
        metadata_prefix: Option<Vec<String>>,
    ) -> Self {
        let user_agent = match user_agent_prefix {
            Some(ua) => format!("{} (rust-tuf/{})", ua, env!("CARGO_PKG_VERSION")),
            None => format!("rust-tuf/{}", env!("CARGO_PKG_VERSION")),
        };

        HttpRepository {
            url,
            client,
            user_agent,
            metadata_prefix,
            interchange: PhantomData,
        }
    }

    fn get(&self, prefix: &Option<Vec<String>>, components: &[String]) -> Result<Response> {
        let mut headers = Headers::new();
        headers.set(UserAgent(self.user_agent.clone()));

        let mut url = self.url.clone();
        {
            let mut segments = url.path_segments_mut().map_err(|_| {
                Error::IllegalArgument(format!("URL was 'cannot-be-a-base': {:?}", self.url))
            })?;
            if let Some(ref prefix) = prefix {
                segments.extend(prefix);
            }
            segments.extend(components);
        }

        let req = self.client.get(url.clone()).headers(headers);
        let resp = req.send()?;

        if !resp.status.is_success() {
            if resp.status == StatusCode::NotFound {
                Err(Error::NotFound)
            } else {
                Err(Error::Opaque(format!(
                    "Error getting {:?}: {:?}",
                    url, resp
                )))
            }
        } else {
            Ok(resp)
        }
    }
}

impl<D> Repository<D> for HttpRepository<D>
where
    D: DataInterchange,
{
    type TargetRead = Response;

    /// This always returns `Err` as storing over HTTP is not yet supported.
    fn store_metadata<M>(
        &self,
        _: &MetadataPath,
        _: &MetadataVersion,
        _: &SignedMetadata<D, M>,
    ) -> Result<()>
    where
        M: Metadata,
    {
        Err(Error::Opaque(
            "Http repo store metadata not implemented".to_string(),
        ))
    }

    fn fetch_metadata<M>(
        &self,
        meta_path: &MetadataPath,
        version: &MetadataVersion,
        max_size: &Option<usize>,
        min_bytes_per_second: u32,
        hash_data: Option<(&HashAlgorithm, HashValue)>,
    ) -> Result<SignedMetadata<D, M>>
    where
        M: Metadata,
    {
        Self::check::<M>(meta_path)?;

        let resp = self.get(&self.metadata_prefix, &meta_path.components::<D>(&version))?;

        let read = SafeReader::new(
            resp,
            max_size.unwrap_or(::std::usize::MAX) as u64,
            min_bytes_per_second,
            hash_data,
        )?;
        Ok(D::from_reader(read)?)
    }

    /// This always returns `Err` as storing over HTTP is not yet supported.
    fn store_target<R>(&self, _: R, _: &TargetPath) -> Result<()>
    where
        R: Read,
    {
        Err(Error::Opaque("Http repo store not implemented".to_string()))
    }

    fn fetch_target(
        &self,
        target_path: &TargetPath,
        target_description: &TargetDescription,
        min_bytes_per_second: u32,
    ) -> Result<SafeReader<Self::TargetRead>> {
        let resp = self.get(&None, &target_path.components())?;
        let (alg, value) = crypto::hash_preference(target_description.hashes())?;
        Ok(SafeReader::new(
            resp,
            target_description.size(),
            min_bytes_per_second,
            Some((alg, value.clone())),
        )?)
    }
}

/// An ephemeral repository contained solely in memory.
pub struct EphemeralRepository<D>
where
    D: DataInterchange,
{
    metadata: Arc<RwLock<HashMap<(MetadataPath, MetadataVersion), Vec<u8>>>>,
    targets: Arc<RwLock<HashMap<TargetPath, Vec<u8>>>>,
    interchange: PhantomData<D>,
}

impl<D> EphemeralRepository<D>
where
    D: DataInterchange,
{
    /// Create a new ephemercal repository.
    pub fn new() -> Self {
        EphemeralRepository {
            metadata: Arc::new(RwLock::new(HashMap::new())),
            targets: Arc::new(RwLock::new(HashMap::new())),
            interchange: PhantomData,
        }
    }
}

impl<D> Default for EphemeralRepository<D>
where
    D: DataInterchange,
{
    fn default() -> Self {
        EphemeralRepository::new()
    }
}

impl<D> Repository<D> for EphemeralRepository<D>
where
    D: DataInterchange,
{
    type TargetRead = Cursor<Vec<u8>>;

    fn store_metadata<M>(
        &self,
        meta_path: &MetadataPath,
        version: &MetadataVersion,
        metadata: &SignedMetadata<D, M>,
    ) -> Result<()>
    where
        M: Metadata,
    {
        Self::check::<M>(meta_path)?;
        let mut buf = Vec::new();
        D::to_writer(&mut buf, metadata)?;
        let mut metadata = self.metadata.write().unwrap();
        let _ = metadata.insert((meta_path.clone(), version.clone()), buf);
        Ok(())
    }

    fn fetch_metadata<M>(
        &self,
        meta_path: &MetadataPath,
        version: &MetadataVersion,
        max_size: &Option<usize>,
        min_bytes_per_second: u32,
        hash_data: Option<(&HashAlgorithm, HashValue)>,
    ) -> Result<SignedMetadata<D, M>>
    where
        M: Metadata,
    {
        Self::check::<M>(meta_path)?;

        let metadata = self.metadata.read().unwrap();
        match metadata.get(&(meta_path.clone(), version.clone())) {
            Some(bytes) => {
                let reader = SafeReader::new(
                    &**bytes,
                    max_size.unwrap_or(::std::usize::MAX) as u64,
                    min_bytes_per_second,
                    hash_data,
                )?;
                D::from_reader(reader)
            }
            None => Err(Error::NotFound),
        }
    }

    fn store_target<R>(&self, mut read: R, target_path: &TargetPath) -> Result<()>
    where
        R: Read,
    {
        let mut buf = Vec::new();
        read.read_to_end(&mut buf)?;
        let mut targets = self.targets.write().unwrap();
        let _ = targets.insert(target_path.clone(), buf);
        Ok(())
    }

    fn fetch_target(
        &self,
        target_path: &TargetPath,
        target_description: &TargetDescription,
        min_bytes_per_second: u32,
    ) -> Result<SafeReader<Self::TargetRead>> {
        let targets = self.targets.read().unwrap();
        match targets.get(target_path) {
            Some(bytes) => {
                let cur = Cursor::new(bytes.clone());
                let (alg, value) = crypto::hash_preference(target_description.hashes())?;
                let read = SafeReader::new(
                    cur,
                    target_description.size(),
                    min_bytes_per_second,
                    Some((alg, value.clone())),
                )?;
                Ok(read)
            }
            None => Err(Error::NotFound),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use interchange::Json;
    use tempfile;

    #[test]
    fn ephemeral_repo_targets() {
        let repo = EphemeralRepository::<Json>::new();

        let data: &[u8] = b"like tears in the rain";
        let target_description =
            TargetDescription::from_reader(data, &[HashAlgorithm::Sha256]).unwrap();
        let path = TargetPath::new("batty".into()).unwrap();
        repo.store_target(data, &path).unwrap();

        let mut read = repo.fetch_target(&path, &target_description, 0).unwrap();
        let mut buf = Vec::new();
        read.read_to_end(&mut buf).unwrap();
        assert_eq!(buf.as_slice(), data);

        let bad_data: &[u8] = b"you're in a desert";
        repo.store_target(bad_data, &path).unwrap();
        let mut read = repo.fetch_target(&path, &target_description, 0).unwrap();
        assert!(read.read_to_end(&mut buf).is_err());
    }

    #[test]
    fn file_system_repo_targets() {
        let temp_dir = tempfile::Builder::new()
            .prefix("rust-tuf")
            .tempdir()
            .unwrap();
        let repo = FileSystemRepository::<Json>::new(temp_dir.path().to_path_buf()).unwrap();

        // test that init worked
        assert!(temp_dir.path().join("metadata").exists());
        assert!(temp_dir.path().join("targets").exists());
        assert!(temp_dir.path().join("temp").exists());

        let data: &[u8] = b"like tears in the rain";
        let target_description =
            TargetDescription::from_reader(data, &[HashAlgorithm::Sha256]).unwrap();
        let path = TargetPath::new("foo/bar/baz".into()).unwrap();
        repo.store_target(data, &path).unwrap();
        assert!(
            temp_dir
                .path()
                .join("targets")
                .join("foo")
                .join("bar")
                .join("baz")
                .exists()
        );

        let mut buf = Vec::new();

        // Enclose `fetch_target` in a scope to make sure the file is closed.
        // This is needed for `tempfile` on Windows, which doesn't open the
        // files in a mode that allows the file to be opened multiple times.
        {
            let mut read = repo.fetch_target(&path, &target_description, 0).unwrap();
            read.read_to_end(&mut buf).unwrap();
            assert_eq!(buf.as_slice(), data);
        }

        let bad_data: &[u8] = b"you're in a desert";
        repo.store_target(bad_data, &path).unwrap();
        let mut read = repo.fetch_target(&path, &target_description, 0).unwrap();
        assert!(read.read_to_end(&mut buf).is_err());
    }
}
