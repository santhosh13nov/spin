//! OCI registries integration.
#![deny(missing_docs)]

use anyhow::{bail, Context, Result};
use docker_credential::DockerCredential;
use oci_distribution::{
    client::{Config, ImageLayer},
    manifest::OciImageManifest,
    secrets::RegistryAuth,
    Reference,
};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use spin_app::locked::{ContentPath, ContentRef};
use spin_loader::oci::cache::Cache;
use spin_manifest::Application;
use tokio::fs;
use walkdir::WalkDir;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

// TODO: the media types for application, wasm module, and data layer are not final.
const SPIN_APPLICATION_MEDIA_TYPE: &str = "application/vnd.fermyon.spin.application.v1+config";
const WASM_LAYER_MEDIA_TYPE: &str = "application/vnd.wasm.content.layer.v1+wasm";
const DATA_MEDIATYPE: &str = "application/vnd.wasm.content.layer.v1+data";

/// Client for interacting with an OCI registry for Spin applications.
pub struct Client {
    /// Global cache for the metadata, Wasm modules, and static assets pulled from OCI registries.
    pub cache: Cache,
    oci: oci_distribution::Client,
}

impl Client {
    /// Create a new instance of an OCI client for distributing Spin applications.
    pub async fn new(insecure: bool, cache_root: Option<PathBuf>) -> Result<Self> {
        let client = oci_distribution::Client::new(Self::build_config(insecure));
        let cache = Cache::new(cache_root).await?;

        Ok(Self { oci: client, cache })
    }

    /// Push a Spin application to an OCI registry.
    pub async fn push(&mut self, app: &Application, reference: impl AsRef<str>) -> Result<()> {
        let reference: Reference = reference
            .as_ref()
            .parse()
            .with_context(|| format!("cannot parse reference {}", reference.as_ref()))?;
        let auth = Self::auth(&reference).await?;
        let working_dir = tempfile::tempdir()?;

        // Create a locked application from the application manifest.
        // TODO: We don't need an extra copy here for each asset to prepare the application.
        // We should be able to use assets::collect instead when constructing the locked app.
        let locked = spin_trigger::locked::build_locked_app(app.clone(), working_dir.path())
            .context("cannot create locked app")?;
        let mut locked = locked.clone();

        // For each component in the application, add layers for the wasm module and
        // all static assets and update the locked application with the file digests.
        let mut layers = Vec::new();
        let mut components = Vec::new();

        for mut c in locked.components {
            // Add the wasm module for the component as layers.
            let source = c
                .clone()
                .source
                .content
                .source
                .context("component loaded from disk should contain a file source")?;

            let source = spin_trigger::parse_file_url(source.as_str())?;
            let layer = Self::wasm_layer(&source).await?;
            let digest = &layer.sha256_digest();
            layers.push(layer);

            // Update the module source with the content digest of the layer.
            c.source.content = ContentRef {
                source: None,
                digest: Some(digest.clone()),
            };

            // Add a layer for each file referenced in the mount directory.
            // Note that this is in fact a directory, and not a single file, so we need to
            // recursively traverse it and add layers for each file.
            let mut files = Vec::new();
            for f in c.files {
                let source = f
                    .content
                    .source
                    .context("file mount loaded from disk should contain a file source")?;
                let source = spin_trigger::parse_file_url(source.as_str())?;
                // Traverse each mount directory, add all static assets as layers, then update the
                // locked application file with the file digest.
                for entry in WalkDir::new(&source) {
                    let entry = entry?;
                    if entry.file_type().is_file() && !entry.file_type().is_dir() {
                        tracing::trace!(
                            "Adding new layer for asset {:?}",
                            spin_loader::to_relative(entry.path(), &source)?
                        );
                        let layer = Self::data_layer(entry.path()).await?;

                        let digest = &layer.sha256_digest();
                        layers.push(layer);

                        files.push(ContentPath {
                            content: ContentRef {
                                source: None,
                                digest: Some(digest.clone()),
                            },
                            path: PathBuf::from(spin_loader::to_relative(entry.path(), &source)?),
                        });
                    }
                }
            }
            c.files = files;
            components.push(c);
        }
        locked.components = components;
        locked.metadata.remove(&"origin".to_string());

        let oci_config = Config {
            data: serde_json::to_vec(&locked)?,
            media_type: SPIN_APPLICATION_MEDIA_TYPE.to_string(),
            annotations: None,
        };
        let manifest = OciImageManifest::build(&layers, &oci_config, None);
        let response = self
            .oci
            .push(&reference, &layers, oci_config, &auth, Some(manifest))
            .await
            .map(|push_response| push_response.manifest_url)
            .context("cannot push Spin application")?;

        tracing::info!("Pushed {:?}", response);

        Ok(())
    }

    /// Pull a Spin application from an OCI registry.
    pub async fn pull(&mut self, reference: &str) -> Result<()> {
        let reference: Reference = reference.parse().context("cannot parse reference")?;
        let auth = Self::auth(&reference).await?;

        // Pull the manifest from the registry.
        let (manifest, digest) = self.oci.pull_image_manifest(&reference, &auth).await?;

        let manifest_json = serde_json::to_string(&manifest)?;
        tracing::debug!("Pulled manifest: {}", manifest_json);

        // Write the manifest in `<cache_root>/registry/oci/manifests/repository:<tag_or_latest>/manifest.json`
        let m = self.cache.oci_manifest_path(&reference.to_string()).await?;
        fs::write(&m, &manifest_json).await?;

        let mut cfg_bytes = Vec::new();
        self.oci
            .pull_blob(&reference, &manifest.config.digest, &mut cfg_bytes)
            .await?;
        let cfg = std::str::from_utf8(&cfg_bytes)?;
        tracing::debug!("Pulled config: {}", cfg);

        // Write the config object in `<cache_root>/registry/oci/manifests/repository:<tag_or_latest>/config.json`
        let c = self.cache.lockfile_path(&reference.to_string()).await?;
        fs::write(&c, &cfg).await?;

        // If a layer is a Wasm module, write it in the Wasm directory.
        // Otherwise, write it in the data directory.
        for layer in manifest.layers {
            // Skip pulling if the digest already exists in the wasm or data directories.
            if self.cache.wasm_file(&layer.digest).is_ok()
                || self.cache.data_file(&layer.digest).is_ok()
            {
                tracing::debug!("Layer {} already exists in cache", &layer.digest);
                continue;
            }
            tracing::debug!("Pulling layer {}", &layer.digest);
            let mut bytes = Vec::new();
            self.oci
                .pull_blob(&reference, &layer.digest, &mut bytes)
                .await?;

            match layer.media_type.as_str() {
                WASM_LAYER_MEDIA_TYPE => self.cache.write_wasm(&bytes, &layer.digest).await?,
                _ => self.cache.write_data(&bytes, &layer.digest).await?,
            }
        }

        tracing::info!("Pulled {}@{}", reference, digest);

        Ok(())
    }

    /// Create a new wasm layer based on a file.
    pub async fn wasm_layer(file: &Path) -> Result<ImageLayer> {
        tracing::log::trace!("Reading wasm module from {:?}", file);
        Ok(ImageLayer::new(
            fs::read(file).await.context("cannot read wasm module")?,
            WASM_LAYER_MEDIA_TYPE.to_string(),
            None,
        ))
    }

    /// Create a new data layer based on a file.
    pub async fn data_layer(file: &Path) -> Result<ImageLayer> {
        tracing::log::trace!("Reading data file from {:?}", file);
        Ok(ImageLayer::new(
            fs::read(&file).await?,
            DATA_MEDIATYPE.to_string(),
            None,
        ))
    }

    /// Save a credential set containing the registry username and password.
    pub async fn login(
        server: impl AsRef<str>,
        username: impl AsRef<str>,
        password: impl AsRef<str>,
    ) -> Result<()> {
        // We want to allow a user to login to both https://ghcr.io and ghcr.io.
        let server = server.as_ref();
        let server = match server.parse::<Url>() {
            Ok(url) => url.host_str().unwrap_or(server).to_string(),
            Err(_) => server.to_string(),
        };

        // First, validate the credentials. If a user accidentally enters a wrong credential set, this
        // can catch the issue early rather than getting an error at the first operation that needs
        // to use the credentials (first time they do a push/pull/up).
        Self::validate_credentials(&server, &username, &password).await?;

        // Save an encoded representation of the credential set in the local configuration file.
        let mut auth = AuthConfig::load_default().await?;
        auth.insert(server, username, password)?;
        auth.save_default().await
    }

    /// Validate the credentials by attempting to send an authenticated request to the registry.
    async fn validate_credentials(
        server: impl AsRef<str>,
        username: impl AsRef<str>,
        password: impl AsRef<str>,
    ) -> Result<()> {
        let client = dkregistry::v2::Client::configure()
            .registry(server.as_ref())
            .insecure_registry(false)
            .username(Some(username.as_ref().into()))
            .password(Some(password.as_ref().into()))
            .build()
            .context("cannot create client to send authentication request to the registry")?;

        match client
            // We don't need to configure any scopes, we are only testing that the credentials are
            // valid for the intended registry.
            .authenticate(&[""])
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => bail!(format!(
                "cannot authenticate as {} to registry {}: {}",
                username.as_ref(),
                server.as_ref(),
                e
            )),
        }
    }

    /// Construct the registry authentication based on the reference.
    async fn auth(reference: &Reference) -> Result<RegistryAuth> {
        let server = reference
            .resolve_registry()
            .strip_suffix('/')
            .unwrap_or_else(|| reference.resolve_registry());

        match AuthConfig::get_auth_from_default(server).await {
            Ok(c) => Ok(c),
            Err(_) => match docker_credential::get_credential(server) {
                Err(e) => {
                    tracing::trace!("Cannot retrieve credentials from Docker, attempting to use anonymous auth: {}", e);
                    Ok(RegistryAuth::Anonymous)
                }

                Ok(DockerCredential::UsernamePassword(username, password)) => {
                    tracing::trace!("Found Docker credentials");
                    Ok(RegistryAuth::Basic(username, password))
                }
                Ok(DockerCredential::IdentityToken(_)) => {
                    tracing::trace!("Cannot use contents of Docker config, identity token not supported. Using anonymous auth");
                    Ok(RegistryAuth::Anonymous)
                }
            },
        }
    }

    /// Build the OCI client configuration given the insecure option.
    fn build_config(insecure: bool) -> oci_distribution::client::ClientConfig {
        let protocol = if insecure {
            oci_distribution::client::ClientProtocol::Http
        } else {
            oci_distribution::client::ClientProtocol::Https
        };

        oci_distribution::client::ClientConfig {
            protocol,
            ..Default::default()
        }
    }
}

#[derive(Serialize, Deserialize)]
struct AuthConfig {
    /// Map between registry server and base64 encoded username:password credential set.
    pub auths: HashMap<String, String>,
}

impl AuthConfig {
    /// Load the authentication configuration from the default location
    /// ($XDG_CONFIG_HOME/fermyon/registry-auth.json).
    pub async fn load_default() -> Result<Self> {
        // TODO: add a way to override this path.
        match Self::load(&Self::default_path()?).await {
            Ok(s) => Ok(s),
            Err(_) => Ok(Self {
                auths: HashMap::new(),
            }),
        }
    }

    /// Save the authentication configuration to the default location
    /// ($XDG_CONFIG_HOME/fermyon/registry-auth.json).
    pub async fn save_default(&self) -> Result<()> {
        self.save(&Self::default_path()?).await
    }

    /// Insert the new credentials into the auths file, with the server as the key and base64
    /// encoded username:password as the value.
    pub fn insert(
        &mut self,
        server: impl AsRef<str>,
        username: impl AsRef<str>,
        password: impl AsRef<str>,
    ) -> Result<()> {
        let encoded = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("{}:{}", username.as_ref(), password.as_ref()),
        );
        self.auths.insert(server.as_ref().to_string(), encoded);

        Ok(())
    }

    fn default_path() -> Result<PathBuf> {
        Ok(dirs::config_dir()
            .context("Cannot find configuration directory")?
            .join("fermyon")
            .join("registry-auth.json"))
    }

    /// Get the registry authentication for a given registry from the default location.
    async fn get_auth_from_default(server: impl AsRef<str>) -> Result<RegistryAuth> {
        let auths = Self::load_default().await?;
        let encoded = match auths.auths.get(&server.as_ref().to_string()) {
            Some(e) => e,
            None => bail!(format!("no credentials stored for {}", server.as_ref())),
        };

        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)?;
        let decoded = std::str::from_utf8(&bytes)?;
        let parts: Vec<&str> = decoded.splitn(2, ':').collect();

        tracing::trace!("Decoded registry credentials from the Spin configuration.");
        Ok(RegistryAuth::Basic(
            parts
                .first()
                .context("expected username as first element of the decoded auth")?
                .to_string(),
            parts
                .get(1)
                .context("expected secret as second element of the decoded auth")?
                .to_string(),
        ))
    }

    async fn load(p: &Path) -> Result<Self> {
        let contents = tokio::fs::read_to_string(&p).await?;
        serde_json::from_str(&contents)
            .with_context(|| format!("cannot load authentication file {:?}", p))
    }

    async fn save(&self, p: &Path) -> Result<()> {
        tokio::fs::write(&p, &serde_json::to_vec_pretty(&self)?)
            .await
            .with_context(|| format!("cannot save authentication file {:?}", p))
    }
}