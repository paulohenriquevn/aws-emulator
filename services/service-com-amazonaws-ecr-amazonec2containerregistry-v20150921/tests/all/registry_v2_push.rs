// SPDX-License-Identifier: CC0-1.0

//! The push half of the Docker Registry v2 / OCI Distribution data plane, end to end over
//! the wire: the requests `docker push` (and BuildKit and containerd) make after
//! `docker login`.
//!
//! The RULE asserted: **a client presenting the `docker login` credential
//! `GetAuthorizationToken` minted can upload a blob into a repository (in chunks, or in one
//! request), check whether a blob is already there, and store a manifest under a tag or a
//! digest; what it stores is the same image record `PutImage` writes, so the control plane
//! (`ListImages`, `DescribeImages`, `BatchGetImage`) reports it and the pull routes serve it
//! back byte for byte.** A blob whose bytes do not hash to the digest the client asserts, a
//! manifest naming a blob or child manifest the repository does not hold, an overwrite of
//! an immutable tag, and a request with no honoured credential are each refused in the
//! registry API's own error document.

use bytes::Bytes;
use serde_json::{json, Value};
use smithy_server_runtime::MultiServiceRouter;

use crate::wire::{at, emulator, layer_digest, ok, REGION, REGISTRY_ID};

/// The Docker image manifest V2 schema 2 media type.
const DOCKER_V2_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// The OCI image manifest media type.
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

/// The OCI image index media type, which BuildKit pushes a multi-platform image as.
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";

/// The Docker image configuration blob media type.
const DOCKER_CONFIG_MEDIA_TYPE: &str = "application/vnd.docker.container.image.v1+json";

/// The Docker gzipped filesystem layer media type.
const DOCKER_LAYER_MEDIA_TYPE: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";

/// The OCI image configuration blob media type.
const OCI_CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.image.config.v1+json";

/// The OCI gzipped filesystem layer media type.
const OCI_LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

/// The registry host every request is addressed to: the endpoint ECR publishes for this
/// registry, which is the host `docker push` uses.
fn registry_host() -> String {
    format!("{REGISTRY_ID}.dkr.ecr.{REGION}.amazonaws.com")
}

/// What one registry request was answered.
struct Answered {
    status: u16,
    headers: ::http::HeaderMap,
    bytes: Bytes,
}

impl Answered {
    /// One response header as text, or the empty string when it is absent.
    fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    /// The body as text, for failure messages.
    fn body(&self) -> String {
        String::from_utf8_lossy(&self.bytes).to_string()
    }

    /// The first error code of the registry error document, or the empty string.
    fn code(&self) -> String {
        serde_json::from_slice::<Value>(&self.bytes)
            .ok()
            .and_then(|document| {
                document["errors"][0]["code"]
                    .as_str()
                    .map(ToString::to_string)
            })
            .unwrap_or_default()
    }
}

/// One request to the registry endpoint as a container client makes it.
async fn send(
    router: &MultiServiceRouter,
    method: &str,
    path: &str,
    credential: Option<&str>,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Answered {
    let mut request = ::http::Request::builder()
        .method(method)
        .uri(path)
        .header("host", registry_host());
    if let Some(credential) = credential {
        request = request.header("authorization", credential);
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request
        .body(Bytes::copy_from_slice(body))
        .expect("the test request is well formed");
    let response = router.dispatch(request).await;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = response.into_body();
    Answered {
        status,
        headers,
        bytes,
    }
}

/// The `docker login` credential, as `Basic <token>`.
async fn given_credential(router: &MultiServiceRouter) -> String {
    let issued = ok(router, "credential", "GetAuthorizationToken", &json!({})).await;
    let token = at(&issued, "/authorizationData/0/authorizationToken")
        .as_str()
        .unwrap_or_else(|| panic!("GetAuthorizationToken must issue a token, got {issued}"))
        .to_string();
    format!("Basic {token}")
}

/// Create `repository`, mutable unless `immutable`.
async fn given_repository(router: &MultiServiceRouter, repository: &str, immutable: bool) {
    let mutability = if immutable { "IMMUTABLE" } else { "MUTABLE" };
    ok(
        router,
        repository,
        "CreateRepository",
        &json!({"repositoryName": repository, "imageTagMutability": mutability}),
    )
    .await;
}

/// The `Location` an answer names, as the path a client follows: relative as served, or
/// the path of an absolute URL.
fn location_path(answered: &Answered) -> String {
    let location = answered.header("location");
    match location.split_once("://") {
        Some((_, rest)) => rest
            .find('/')
            .map_or_else(String::new, |at| rest[at..].to_string()),
        None => location,
    }
}

/// Upload `content` into `repository` the way `docker push` streams a layer: open a session,
/// PATCH it in two chunks, then PUT with the digest. Asserts every step's answer and
/// answers the digest.
async fn push_blob_in_chunks(
    router: &MultiServiceRouter,
    credential: &str,
    repository: &str,
    content: &[u8],
) -> String {
    let digest = layer_digest(content);
    let opened = send(
        router,
        "POST",
        &format!("/v2/{repository}/blobs/uploads/"),
        Some(credential),
        &[],
        &[],
    )
    .await;
    assert_eq!(opened.status, 202, "opening an upload: {}", opened.body());
    let mut session = location_path(&opened);
    let upload_uuid = opened.header("docker-upload-uuid");
    assert!(!upload_uuid.is_empty(), "an upload must name its Docker-Upload-UUID");
    assert_eq!(
        session,
        format!("/v2/{repository}/blobs/uploads/{upload_uuid}"),
        "the Location must address the session the UUID names"
    );
    assert_eq!(opened.header("range"), "0-0", "a new session has received nothing");

    let split = content.len() / 2;
    let chunks = [&content[..split], &content[split..]];
    let mut offset = 0usize;
    for chunk in chunks {
        let range = format!("{}-{}", offset, offset + chunk.len() - 1);
        let patched = send(
            router,
            "PATCH",
            &session,
            Some(credential),
            &[
                ("content-type", "application/octet-stream"),
                ("content-range", &range),
            ],
            chunk,
        )
        .await;
        assert_eq!(patched.status, 202, "PATCH {range}: {}", patched.body());
        offset += chunk.len();
        assert_eq!(
            patched.header("range"),
            format!("0-{}", offset - 1),
            "the Range must state every byte received so far"
        );
        assert_eq!(patched.header("docker-upload-uuid"), upload_uuid);
        session = location_path(&patched);
    }

    let status = send(router, "GET", &session, Some(credential), &[], &[]).await;
    assert_eq!(status.status, 204, "upload status: {}", status.body());
    assert_eq!(status.header("range"), format!("0-{}", content.len() - 1));

    let finished = send(
        router,
        "PUT",
        &format!("{session}?digest={digest}"),
        Some(credential),
        &[],
        &[],
    )
    .await;
    assert_eq!(finished.status, 201, "finishing the upload: {}", finished.body());
    assert_eq!(finished.header("docker-content-digest"), digest);
    assert_eq!(
        location_path(&finished),
        format!("/v2/{repository}/blobs/{digest}")
    );
    digest
}

/// Upload `content` in ONE request: `POST ...?digest=` carrying the whole blob.
async fn push_blob_monolithic(
    router: &MultiServiceRouter,
    credential: &str,
    repository: &str,
    content: &[u8],
) -> String {
    let digest = layer_digest(content);
    // A client percent-encodes the `:` of the digest in the query.
    let encoded = digest.replace(':', "%3A");
    let pushed = send(
        router,
        "POST",
        &format!("/v2/{repository}/blobs/uploads/?digest={encoded}"),
        Some(credential),
        &[("content-type", "application/octet-stream")],
        content,
    )
    .await;
    assert_eq!(pushed.status, 201, "monolithic upload: {}", pushed.body());
    assert_eq!(pushed.header("docker-content-digest"), digest);
    assert_eq!(
        location_path(&pushed),
        format!("/v2/{repository}/blobs/{digest}")
    );
    digest
}

/// A schema-2 image manifest over `config` and `layer`.
fn docker_manifest(config: (&str, usize), layer: (&str, usize)) -> String {
    json!({
        "schemaVersion": 2,
        "mediaType": DOCKER_V2_MANIFEST,
        "config": {"mediaType": DOCKER_CONFIG_MEDIA_TYPE, "digest": config.0, "size": config.1},
        "layers": [{"mediaType": DOCKER_LAYER_MEDIA_TYPE, "digest": layer.0, "size": layer.1}],
    })
    .to_string()
}

/// PUT `manifest` under `reference`, with `media_type` as its Content-Type.
async fn put_manifest(
    router: &MultiServiceRouter,
    credential: Option<&str>,
    repository: &str,
    reference: &str,
    media_type: &str,
    manifest: &str,
) -> Answered {
    send(
        router,
        "PUT",
        &format!("/v2/{repository}/manifests/{reference}"),
        credential,
        &[("content-type", media_type)],
        manifest.as_bytes(),
    )
    .await
}

/// A chunked push of a config blob and a layer, then a schema-2 manifest by tag: the image
/// is what the control plane reports and what the pull routes serve back, byte for byte.
#[tokio::test]
async fn a_chunked_push_stores_an_image_the_control_plane_reports_and_a_pull_serves() {
    let router = emulator();
    let credential = given_credential(&router).await;
    let repository = "team/push-probe";
    given_repository(&router, repository, false).await;

    let config_bytes = b"{\"architecture\":\"amd64\",\"os\":\"linux\"}".to_vec();
    let layer_bytes: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let config = push_blob_in_chunks(&router, &credential, repository, &config_bytes).await;
    let layer = push_blob_in_chunks(&router, &credential, repository, &layer_bytes).await;

    let manifest = docker_manifest(
        (&config, config_bytes.len()),
        (&layer, layer_bytes.len()),
    );
    let manifest_digest = layer_digest(manifest.as_bytes());
    let stored = put_manifest(
        &router,
        Some(&credential),
        repository,
        "v1",
        DOCKER_V2_MANIFEST,
        &manifest,
    )
    .await;
    assert_eq!(stored.status, 201, "manifest PUT: {}", stored.body());
    assert_eq!(stored.header("docker-content-digest"), manifest_digest);
    assert_eq!(
        location_path(&stored),
        format!("/v2/{repository}/manifests/{manifest_digest}")
    );

    // The control plane reports the pushed image.
    let listed = ok(
        &router,
        "list",
        "ListImages",
        &json!({"repositoryName": repository}),
    )
    .await;
    let ids = listed["imageIds"].as_array().cloned().unwrap_or_default();
    assert!(
        ids.contains(&json!({"imageDigest": manifest_digest, "imageTag": "v1"})),
        "ListImages must report the pushed image, got {listed}"
    );
    let described = ok(
        &router,
        "describe",
        "DescribeImages",
        &json!({"repositoryName": repository}),
    )
    .await;
    assert_eq!(at(&described, "/imageDetails/0/imageDigest"), json!(manifest_digest));
    assert_eq!(at(&described, "/imageDetails/0/imageTags"), json!(["v1"]));
    assert_eq!(
        at(&described, "/imageDetails/0/imageManifestMediaType"),
        json!(DOCKER_V2_MANIFEST)
    );
    let fetched = ok(
        &router,
        "batch get",
        "BatchGetImage",
        &json!({"repositoryName": repository, "imageIds": [{"imageTag": "v1"}]}),
    )
    .await;
    assert_eq!(at(&fetched, "/images/0/imageManifest"), json!(manifest));

    // The pull routes serve it back byte for byte.
    let pulled = send(
        &router,
        "GET",
        &format!("/v2/{repository}/manifests/v1"),
        Some(&credential),
        &[("accept", DOCKER_V2_MANIFEST)],
        &[],
    )
    .await;
    assert_eq!(pulled.status, 200, "manifest GET: {}", pulled.body());
    assert_eq!(pulled.bytes.as_ref(), manifest.as_bytes());
    assert_eq!(pulled.header("docker-content-digest"), manifest_digest);
    for (digest, bytes) in [(&config, &config_bytes), (&layer, &layer_bytes)] {
        let blob = send(
            &router,
            "GET",
            &format!("/v2/{repository}/blobs/{digest}"),
            Some(&credential),
            &[],
            &[],
        )
        .await;
        assert_eq!(blob.status, 200, "blob GET {digest}: {}", blob.body());
        assert_eq!(blob.bytes.as_ref(), bytes.as_slice(), "blob {digest} bytes");
    }
}

/// `POST ...?digest=` with the whole blob is a finished upload in one request.
#[tokio::test]
async fn a_monolithic_upload_stores_the_blob_in_one_request() {
    let router = emulator();
    let credential = given_credential(&router).await;
    given_repository(&router, "monolithic-probe", false).await;
    let content = b"one request carries the whole blob".to_vec();

    let digest = push_blob_monolithic(&router, &credential, "monolithic-probe", &content).await;

    let blob = send(
        &router,
        "GET",
        &format!("/v2/monolithic-probe/blobs/{digest}"),
        Some(&credential),
        &[],
        &[],
    )
    .await;
    assert_eq!(blob.status, 200, "blob GET: {}", blob.body());
    assert_eq!(blob.bytes.as_ref(), content.as_slice());

    // The blob is the same record the control plane reports availability from.
    let checked = ok(
        &router,
        "availability",
        "BatchCheckLayerAvailability",
        &json!({"repositoryName": "monolithic-probe", "layerDigests": [digest]}),
    )
    .await;
    assert_eq!(at(&checked, "/layers/0/layerAvailability"), json!("AVAILABLE"));
}

/// A finish whose digest is not the digest of the bytes received is refused, and the blob
/// is not stored.
#[tokio::test]
async fn a_finish_with_a_mismatched_digest_is_refused_as_digest_invalid() {
    let router = emulator();
    let credential = given_credential(&router).await;
    given_repository(&router, "digest-probe", false).await;
    let content = b"the bytes actually sent".to_vec();
    let claimed = layer_digest(b"some other bytes");

    let opened = send(
        &router,
        "POST",
        "/v2/digest-probe/blobs/uploads/",
        Some(&credential),
        &[],
        &[],
    )
    .await;
    assert_eq!(opened.status, 202, "opening an upload: {}", opened.body());
    let session = location_path(&opened);
    let finished = send(
        &router,
        "PUT",
        &format!("{session}?digest={claimed}"),
        Some(&credential),
        &[("content-type", "application/octet-stream")],
        &content,
    )
    .await;
    assert_eq!(finished.status, 400, "mismatched finish: {}", finished.body());
    assert_eq!(finished.code(), "DIGEST_INVALID", "{}", finished.body());

    // Neither digest addresses a stored blob.
    for digest in [claimed, layer_digest(&content)] {
        let probed = send(
            &router,
            "HEAD",
            &format!("/v2/digest-probe/blobs/{digest}"),
            Some(&credential),
            &[],
            &[],
        )
        .await;
        assert_eq!(probed.status, 404, "blob {digest} must not be stored");
    }
}

/// A manifest naming a blob the repository does not hold is refused, and nothing is stored.
#[tokio::test]
async fn a_manifest_referencing_a_missing_blob_is_refused_as_manifest_blob_unknown() {
    let router = emulator();
    let credential = given_credential(&router).await;
    given_repository(&router, "missing-blob-probe", false).await;
    let config_bytes = b"{\"os\":\"linux\"}".to_vec();
    let config = push_blob_monolithic(&router, &credential, "missing-blob-probe", &config_bytes)
        .await;
    let absent_layer = layer_digest(b"a layer nobody uploaded");

    let manifest = docker_manifest((&config, config_bytes.len()), (&absent_layer, 23));
    let refused = put_manifest(
        &router,
        Some(&credential),
        "missing-blob-probe",
        "v1",
        DOCKER_V2_MANIFEST,
        &manifest,
    )
    .await;
    assert_eq!(refused.status, 400, "manifest PUT: {}", refused.body());
    assert_eq!(refused.code(), "MANIFEST_BLOB_UNKNOWN", "{}", refused.body());

    let listed = ok(
        &router,
        "list",
        "ListImages",
        &json!({"repositoryName": "missing-blob-probe"}),
    )
    .await;
    assert_eq!(listed["imageIds"], json!([]), "nothing may be stored: {listed}");
}

/// The existence check `docker push` makes before uploading a layer: 404 while the
/// repository does not hold it, 200 with its length and digest once it does.
#[tokio::test]
async fn a_blob_head_answers_404_before_and_200_after_the_upload() {
    let router = emulator();
    let credential = given_credential(&router).await;
    given_repository(&router, "head-probe", false).await;
    let content = b"layer bytes the HEAD reports on".to_vec();
    let digest = layer_digest(&content);
    let path = format!("/v2/head-probe/blobs/{digest}");

    let before = send(&router, "HEAD", &path, Some(&credential), &[], &[]).await;
    assert_eq!(before.status, 404, "before the upload: {}", before.body());

    push_blob_in_chunks(&router, &credential, "head-probe", &content).await;

    let after = send(&router, "HEAD", &path, Some(&credential), &[], &[]).await;
    assert_eq!(after.status, 200, "after the upload");
    assert_eq!(after.header("content-length"), content.len().to_string());
    assert_eq!(after.header("docker-content-digest"), digest);
    assert!(after.bytes.is_empty(), "a HEAD carries no body");
}

/// BuildKit pushes each platform's manifest by digest, then the index over them by tag.
#[tokio::test]
async fn an_image_index_over_pushed_child_manifests_is_stored_and_served() {
    let router = emulator();
    let credential = given_credential(&router).await;
    let repository = "index-probe";
    given_repository(&router, repository, false).await;

    let mut children = Vec::new();
    for platform in ["amd64", "arm64"] {
        let config_bytes =
            format!("{{\"architecture\":\"{platform}\",\"os\":\"linux\"}}").into_bytes();
        let layer_bytes = format!("{platform} filesystem").into_bytes();
        let config = push_blob_monolithic(&router, &credential, repository, &config_bytes).await;
        let layer = push_blob_in_chunks(&router, &credential, repository, &layer_bytes).await;
        let manifest = json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST,
            "config": {"mediaType": OCI_CONFIG_MEDIA_TYPE, "digest": config, "size": config_bytes.len()},
            "layers": [{"mediaType": OCI_LAYER_MEDIA_TYPE, "digest": layer, "size": layer_bytes.len()}],
        })
        .to_string();
        let digest = layer_digest(manifest.as_bytes());
        let stored =
            put_manifest(&router, Some(&credential), repository, &digest, OCI_MANIFEST, &manifest)
                .await;
        assert_eq!(stored.status, 201, "child {platform}: {}", stored.body());
        assert_eq!(stored.header("docker-content-digest"), digest);
        children.push((platform, digest, manifest.len()));
    }

    let index = json!({
        "schemaVersion": 2,
        "mediaType": OCI_INDEX,
        "manifests": children.iter().map(|(platform, digest, size)| json!({
            "mediaType": OCI_MANIFEST,
            "digest": digest,
            "size": size,
            "platform": {"architecture": platform, "os": "linux"},
        })).collect::<Vec<Value>>(),
    })
    .to_string();
    let index_digest = layer_digest(index.as_bytes());
    let stored = put_manifest(&router, Some(&credential), repository, "multi", OCI_INDEX, &index)
        .await;
    assert_eq!(stored.status, 201, "index PUT: {}", stored.body());
    assert_eq!(stored.header("docker-content-digest"), index_digest);

    let pulled = send(
        &router,
        "GET",
        &format!("/v2/{repository}/manifests/multi"),
        Some(&credential),
        &[("accept", OCI_INDEX)],
        &[],
    )
    .await;
    assert_eq!(pulled.status, 200, "index GET: {}", pulled.body());
    assert_eq!(pulled.bytes.as_ref(), index.as_bytes());

    // An index naming a child manifest the repository does not hold is refused.
    let dangling = json!({
        "schemaVersion": 2,
        "mediaType": OCI_INDEX,
        "manifests": [{
            "mediaType": OCI_MANIFEST,
            "digest": layer_digest(b"a manifest nobody pushed"),
            "size": 24,
        }],
    })
    .to_string();
    let refused = put_manifest(&router, Some(&credential), repository, "dangling", OCI_INDEX, &dangling)
        .await;
    assert_eq!(refused.status, 400, "dangling index: {}", refused.body());
    assert_eq!(refused.code(), "MANIFEST_BLOB_UNKNOWN", "{}", refused.body());
}

/// A push presenting no honoured credential is challenged exactly as a pull is.
#[tokio::test]
async fn an_unauthenticated_push_gets_the_same_challenge_as_a_pull() {
    let router = emulator();
    given_repository(&router, "auth-probe", false).await;
    let digest = layer_digest(b"anything");

    let pull = send(&router, "GET", "/v2/auth-probe/manifests/v1", None, &[], &[]).await;
    assert_eq!(pull.status, 401, "unauthenticated pull: {}", pull.body());

    let attempts: Vec<(&str, String, Vec<u8>)> = vec![
        ("POST", "/v2/auth-probe/blobs/uploads/".to_string(), Vec::new()),
        (
            "POST",
            format!("/v2/auth-probe/blobs/uploads/?digest={digest}"),
            b"anything".to_vec(),
        ),
        (
            "PATCH",
            "/v2/auth-probe/blobs/uploads/0000-not-a-session".to_string(),
            b"anything".to_vec(),
        ),
        (
            "PUT",
            format!("/v2/auth-probe/blobs/uploads/0000-not-a-session?digest={digest}"),
            Vec::new(),
        ),
        ("PUT", "/v2/auth-probe/manifests/v1".to_string(), b"{}".to_vec()),
        ("HEAD", format!("/v2/auth-probe/blobs/{digest}"), Vec::new()),
    ];
    for (method, path, body) in attempts {
        let answered = send(&router, method, &path, None, &[], &body).await;
        assert_eq!(answered.status, pull.status, "{method} {path}: {}", answered.body());
        assert_eq!(answered.header("www-authenticate"), pull.header("www-authenticate"));
        if method != "HEAD" {
            assert_eq!(answered.bytes, pull.bytes, "{method} {path}: the same error document");
        }

        let wrong = send(
            &router,
            method,
            &path,
            Some("Basic QVdTOm5vdC1taW50ZWQtaGVyZQ=="),
            &[],
            &body,
        )
        .await;
        assert_eq!(wrong.status, 401, "{method} {path} with a credential never minted");
    }

    let listed = ok(&router, "list", "ListImages", &json!({"repositoryName": "auth-probe"})).await;
    assert_eq!(listed["imageIds"], json!([]), "nothing may be stored: {listed}");
}

/// In an IMMUTABLE repository, a tag that already names one manifest cannot be moved to
/// another; re-pushing the very same manifest under it is not an overwrite.
#[tokio::test]
async fn an_immutable_tag_cannot_be_moved_to_a_different_manifest() {
    let router = emulator();
    let credential = given_credential(&router).await;
    let repository = "immutable-probe";
    given_repository(&router, repository, true).await;

    let config_bytes = b"{\"os\":\"linux\"}".to_vec();
    let config = push_blob_monolithic(&router, &credential, repository, &config_bytes).await;
    let first_layer = b"first layer".to_vec();
    let second_layer = b"second layer".to_vec();
    let first = push_blob_monolithic(&router, &credential, repository, &first_layer).await;
    let second = push_blob_monolithic(&router, &credential, repository, &second_layer).await;
    let original = docker_manifest((&config, config_bytes.len()), (&first, first_layer.len()));
    let replacement =
        docker_manifest((&config, config_bytes.len()), (&second, second_layer.len()));

    let stored =
        put_manifest(&router, Some(&credential), repository, "v1", DOCKER_V2_MANIFEST, &original)
            .await;
    assert_eq!(stored.status, 201, "first push: {}", stored.body());

    let again =
        put_manifest(&router, Some(&credential), repository, "v1", DOCKER_V2_MANIFEST, &original)
            .await;
    assert_eq!(again.status, 201, "re-pushing the same manifest: {}", again.body());

    let refused = put_manifest(
        &router,
        Some(&credential),
        repository,
        "v1",
        DOCKER_V2_MANIFEST,
        &replacement,
    )
    .await;
    assert_eq!(refused.status, 400, "moving an immutable tag: {}", refused.body());
    assert_eq!(refused.code(), "TAG_INVALID", "{}", refused.body());

    let pulled = send(
        &router,
        "GET",
        &format!("/v2/{repository}/manifests/v1"),
        Some(&credential),
        &[("accept", DOCKER_V2_MANIFEST)],
        &[],
    )
    .await;
    assert_eq!(pulled.bytes.as_ref(), original.as_bytes(), "the tag still names the original");
}

/// A lifecycle policy document a creation template carries, already in the minified form
/// ECR reports a policy in.
const TEMPLATE_LIFECYCLE_POLICY: &str = "{\"rules\":[{\"rulePriority\":1,\"description\":\"expire untagged images\",\"selection\":{\"tagStatus\":\"untagged\",\"countType\":\"sinceImagePushed\",\"countUnit\":\"days\",\"countNumber\":14},\"action\":{\"type\":\"expire\"}}]}";

/// Store a repository creation template under `prefix`, applied for `applied_for`.
async fn given_template(
    router: &MultiServiceRouter,
    prefix: &str,
    applied_for: &[&str],
    mutability: &str,
) {
    ok(
        router,
        prefix,
        "CreateRepositoryCreationTemplate",
        &json!({
            "prefix": prefix,
            "appliedFor": applied_for,
            "imageTagMutability": mutability,
            "lifecyclePolicy": TEMPLATE_LIFECYCLE_POLICY,
            "resourceTags": [{"Key": "created-by", "Value": prefix}],
        }),
    )
    .await;
}

/// Push one image (one config blob, one layer, a manifest under `tag`) to `repository`
/// through the registry endpoint, answering the manifest PUT's status.
async fn push_image(
    router: &MultiServiceRouter,
    credential: &str,
    repository: &str,
    tag: &str,
) -> Answered {
    let config_bytes = b"{\"os\":\"linux\"}".to_vec();
    let layer_bytes = format!("{repository} layer").into_bytes();
    let opened = send(
        router,
        "POST",
        &format!("/v2/{repository}/blobs/uploads/"),
        Some(credential),
        &[],
        &[],
    )
    .await;
    if opened.status != 202 {
        return opened;
    }
    let config = push_blob_monolithic(router, credential, repository, &config_bytes).await;
    let layer = push_blob_in_chunks(router, credential, repository, &layer_bytes).await;
    let manifest = docker_manifest((&config, config_bytes.len()), (&layer, layer_bytes.len()));
    put_manifest(router, Some(credential), repository, tag, DOCKER_V2_MANIFEST, &manifest).await
}

/// A push to a repository the registry does not hold creates it from the CREATE_ON_PUSH
/// template whose prefix matches, with that template's settings, and stores the image.
#[tokio::test]
async fn a_push_to_a_missing_repository_creates_it_from_a_matching_create_on_push_template() {
    let router = emulator();
    let credential = given_credential(&router).await;
    given_template(&router, "loopzero-tenants", &["CREATE_ON_PUSH"], "IMMUTABLE").await;
    let repository = "loopzero-tenants/tn-abc/hello-web";

    let stored = push_image(&router, &credential, repository, "v1").await;
    assert_eq!(stored.status, 201, "push to a templated namespace: {}", stored.body());

    let described = ok(
        &router,
        "describe",
        "DescribeRepositories",
        &json!({"repositoryNames": [repository]}),
    )
    .await;
    assert_eq!(
        at(&described, "/repositories/0/imageTagMutability"),
        json!("IMMUTABLE"),
        "the template's tag mutability: {described}"
    );
    let policy = ok(
        &router,
        "lifecycle",
        "GetLifecyclePolicy",
        &json!({"repositoryName": repository}),
    )
    .await;
    assert_eq!(policy["lifecyclePolicyText"], json!(TEMPLATE_LIFECYCLE_POLICY));
    let arn = at(&described, "/repositories/0/repositoryArn");
    let tags = ok(
        &router,
        "tags",
        "ListTagsForResource",
        &json!({"resourceArn": arn}),
    )
    .await;
    assert_eq!(tags["tags"], json!([{"Key": "created-by", "Value": "loopzero-tenants"}]));
    let listed = ok(&router, "list", "ListImages", &json!({"repositoryName": repository})).await;
    assert_eq!(
        at(&listed, "/imageIds/0/imageTag"),
        json!("v1"),
        "the pushed image is listed: {listed}"
    );
}

/// With no template applied for CREATE_ON_PUSH matching the name, a push to a missing
/// repository creates nothing and is refused as naming an unknown repository.
#[tokio::test]
async fn a_push_to_a_missing_repository_without_a_create_on_push_template_is_refused() {
    let router = emulator();
    let credential = given_credential(&router).await;
    // A template for another namespace, and one for this namespace applied only to pull
    // through cache: neither applies to this push.
    given_template(&router, "other-team", &["CREATE_ON_PUSH"], "MUTABLE").await;
    given_template(&router, "cache-only", &["PULL_THROUGH_CACHE"], "MUTABLE").await;

    for repository in ["no-template/app", "cache-only/app", "other-team-lookalike/app"] {
        let refused = push_image(&router, &credential, repository, "v1").await;
        assert_eq!(refused.status, 404, "{repository}: {}", refused.body());
        assert_eq!(refused.code(), "NAME_UNKNOWN", "{repository}: {}", refused.body());
        let manifest = put_manifest(
            &router,
            Some(&credential),
            repository,
            "v1",
            DOCKER_V2_MANIFEST,
            "{\"schemaVersion\":2}",
        )
        .await;
        assert_eq!(manifest.code(), "NAME_UNKNOWN", "{repository}: {}", manifest.body());
    }
    let described = ok(&router, "describe", "DescribeRepositories", &json!({})).await;
    assert_eq!(described["repositories"], json!([]), "nothing may be created: {described}");
}

/// The most specific matching prefix wins over the ROOT catch-all.
#[tokio::test]
async fn the_most_specific_create_on_push_template_wins_over_root() {
    let router = emulator();
    let credential = given_credential(&router).await;
    given_template(&router, "ROOT", &["CREATE_ON_PUSH"], "MUTABLE").await;
    given_template(&router, "prod", &["CREATE_ON_PUSH"], "IMMUTABLE").await;
    given_template(&router, "prod/payments", &["CREATE_ON_PUSH", "REPLICATION"], "MUTABLE")
        .await;

    // (repository, the template that must have created it, its tag mutability)
    let rows = [
        ("prod/payments/api", "prod/payments", "MUTABLE"),
        ("prod/web", "prod", "IMMUTABLE"),
        ("staging/web", "ROOT", "MUTABLE"),
    ];
    for (repository, template, mutability) in rows {
        let stored = push_image(&router, &credential, repository, "v1").await;
        assert_eq!(stored.status, 201, "{repository}: {}", stored.body());
        let described = ok(
            &router,
            repository,
            "DescribeRepositories",
            &json!({"repositoryNames": [repository]}),
        )
        .await;
        assert_eq!(
            at(&described, "/repositories/0/imageTagMutability"),
            json!(mutability),
            "{repository} must be created from the {template:?} template"
        );
        let tags = ok(
            &router,
            repository,
            "ListTagsForResource",
            &json!({"resourceArn": at(&described, "/repositories/0/repositoryArn")}),
        )
        .await;
        assert_eq!(
            tags["tags"],
            json!([{"Key": "created-by", "Value": template}]),
            "{repository} must carry the {template:?} template's tags"
        );
    }
}
