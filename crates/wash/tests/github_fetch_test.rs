// #![cfg(target_os = "linux")]
// NOTE: These are only run on linux for CI purposes, because they rely on the docker client being
// available, and for various reasons this has proven to be problematic on both the Windows and
// MacOS runners we use.

use tempfile::tempdir;
use tokio::io::AsyncBufReadExt;
use wasmcloud_test_util::env::EnvVarGuard;
use wasmcloud_test_util::testcontainers::{AsyncRunner as _, ImageExt, Mount, SquidProxy};

use wash::lib::start::{
    get_download_client, new_patch_or_pre_1_0_0_minor_version_after_version_string,
    new_patch_releases_after, DOWNLOAD_CLIENT_USER_AGENT,
};

// For squid config reference, see: https://www.squid-cache.org/Doc/config/
// Sets up a squid-proxy listening on port 3128 that requires basic auth
const SQUID_CONFIG_WITH_BASIC_AUTH: &str = r#"
# Listen on port 3128 for traffic, allows proxy to run as http endpoint,
# while still serving responses for both HTTP_PROXY and HTTPS_PROXY.
http_port 3128
# log to stdout to make the logs accessible
logfile_rotate 0
# This format translates to: <request-method>|<url>|<return-code>|<user-agent>|<basic-auth-username>
logformat wasmcloud %rm|%ru|%>Hs|%{User-Agent}>h|%[un
cache_log stdio:/dev/stdout
access_log stdio:/dev/stderr wasmcloud
cache_store_log stdio:/dev/stdout
# This set of directives tells squid to require basic auth,
# but the passed in credentials can be whatever to make testing easier.
auth_param basic program /usr/libexec/basic_fake_auth
acl authenticated proxy_auth REQUIRED
http_access allow authenticated
http_access deny all
shutdown_lifetime 1 seconds
"#;

// Sets up a squid-proxy listening on port 3128 that does not require any auth
const SQUID_CONFIG_WITHOUT_AUTH: &str = r#"
http_port 3128
# log to stdout to make the logs accessible
logfile_rotate 0
logformat wasmcloud %rm|%ru|%>Hs|%{User-Agent}>h|%[un
cache_log stdio:/dev/stdout
access_log stdio:/dev/stderr wasmcloud
cache_store_log stdio:/dev/stdout
# Log query params
strip_query_terms off
# allow unauthenticated http(s) access
http_access allow all
shutdown_lifetime 1 seconds
"#;

#[tokio::test]
#[cfg_attr(not(docker_available), ignore = "docker isn't available")]
async fn test_download_client_with_proxy_settings() {
    // NOTE: This is intentional to avoid the two tests running in parallel
    // and contaminating each other's environment variables for configuring
    // the http client based on the environment.
    test_http_proxy_without_auth().await;
    test_http_proxy_with_basic_auth().await;
}

async fn test_http_proxy_without_auth() {
    let dir_path = tempdir().expect("Couldn't create tempdir");

    let squid_config_path = dir_path.path().join("squid.conf");
    tokio::fs::write(squid_config_path.clone(), SQUID_CONFIG_WITHOUT_AUTH)
        .await
        .unwrap();

    let container = SquidProxy::default()
        .with_mount(Mount::bind_mount(
            squid_config_path.to_string_lossy().to_string(),
            "/etc/squid.conf",
        ))
        .start()
        .await
        .expect("failed to start squid-proxy container");

    let proxy_val = format!(
        "http://localhost:{}",
        container
            .get_host_port_ipv4(3128)
            .await
            .expect("failed to get squid-proxy host port")
    );
    let _http_proxy_var = EnvVarGuard::set("HTTP_PROXY", &proxy_val);
    let _https_proxy_var = EnvVarGuard::set("HTTPS_PROXY", &proxy_val);

    let client = get_download_client().unwrap();
    let http_endpoint = "http://httpbin.org/get";
    let https_endpoint = "https://httpbin.org/get";
    let http = client.get(http_endpoint).send().await.unwrap();
    let https = client.get(https_endpoint).send().await.unwrap();

    let _ = container.stop().await;

    assert_eq!(http.status(), reqwest::StatusCode::OK);
    assert_eq!(https.status(), reqwest::StatusCode::OK);

    let mut stderr = vec![];
    let mut lines = container.stderr(false).lines();
    while let Some(line) = lines.next_line().await.unwrap() {
        stderr.push(line);
    }

    // GET|http://httpbin.org/get|200|wash-lib/0.21.1|-
    let http_log_entry = format!("GET|{http_endpoint}|200|{DOWNLOAD_CLIENT_USER_AGENT}|-");
    assert!(
        stderr.contains(&http_log_entry),
        "Didn't find connection log entry, logs:\n {}",
        stderr.join("\n")
    );

    // CONNECT|httpbin.org:443|200|wash-lib/0.21.1|-
    let https_url = url::Url::parse(https_endpoint).unwrap();
    let https_log_entry = format!(
        "CONNECT|{}:{}|200|{}|-",
        https_url.host_str().unwrap(),
        https_url.port_or_known_default().unwrap(),
        DOWNLOAD_CLIENT_USER_AGENT
    );
    assert!(stderr.contains(&https_log_entry));
}

async fn test_http_proxy_with_basic_auth() {
    let dir_path = tempdir().expect("Couldn't create tempdir");

    let squid_config_path = dir_path.path().join("squid.conf");
    tokio::fs::write(squid_config_path.clone(), SQUID_CONFIG_WITH_BASIC_AUTH)
        .await
        .unwrap();

    let container = SquidProxy::default()
        .with_mount(Mount::bind_mount(
            squid_config_path.to_string_lossy().to_string(),
            "/etc/squid.conf",
        ))
        .start()
        .await
        .expect("failed to start squid-proxy container");

    let proxy_val = format!(
        "http://localhost:{}",
        container
            .get_host_port_ipv4(3128)
            .await
            .expect("failed to get squid-proxy host port")
    );
    let _http_proxy_var = EnvVarGuard::set("HTTP_PROXY", &proxy_val);
    let _https_proxy_var = EnvVarGuard::set("HTTPS_PROXY", &proxy_val);

    let proxy_username = "wasmcloud";
    let _proxy_username = EnvVarGuard::set("WASH_PROXY_USERNAME", proxy_username);
    let _proxy_password = EnvVarGuard::set("WASH_PROXY_PASSWORD", "this-can-be-whatever");

    let client = get_download_client().unwrap();
    let http_endpoint = "http://httpbin.org/get";
    let https_endpoint = "https://httpbin.org/get";
    let http = client.get(http_endpoint).send().await.unwrap();
    let https = client.get(https_endpoint).send().await.unwrap();

    let _ = container.stop().await;

    assert_eq!(http.status(), reqwest::StatusCode::OK);
    assert_eq!(https.status(), reqwest::StatusCode::OK);

    let mut stderr = vec![];
    let mut lines = container.stderr(false).lines();
    while let Some(line) = lines.next_line().await.unwrap() {
        stderr.push(line);
    }

    // GET|http://httpbin.org/get|200|wash-lib/0.21.1|wasmcloud
    let http_log_entry =
        format!("GET|{http_endpoint}|200|{DOWNLOAD_CLIENT_USER_AGENT}|{proxy_username}");
    assert!(
        stderr.contains(&http_log_entry),
        "Didn't find connection log entry, logs:\n {}",
        stderr.join("\n")
    );

    // CONNECT|httpbin.org:443|200|wash-lib/0.21.1|wasmcloud
    let https_url = url::Url::parse(https_endpoint).unwrap();
    let https_log_entry = format!(
        "CONNECT|{}:{}|200|{}|{proxy_username}",
        https_url.host_str().unwrap(),
        https_url.port_or_known_default().unwrap(),
        DOWNLOAD_CLIENT_USER_AGENT
    );
    assert!(stderr.contains(&https_log_entry));
}

/// Test if the GitHubRelease struct is parsed correctly from the raw string.
/// Using an already "outdated" patch version to test if the sorting works correctly and comparable to the current version.
#[tokio::test]
#[cfg_attr(not(can_reach_github_com), ignore = "github.com is not reachable")]
async fn test_fetching_wasm_cloud_patch_versions_after_v_1_0_3() {
    let owner = "wasmCloud";
    let repo = "wasmCloud";
    let latest_version = semver::Version::new(1, 0, 3);
    // Use 1.0.3 as the latest version, since there is a newer version
    let patch_releases = new_patch_releases_after(owner, repo, &latest_version)
        .await
        .expect("Should have been able to fetch releases");

    // Checks that the tag name starts with a v
    let re = regex::Regex::new(r"^v\d+\.\d+\.\d+").unwrap();

    for new_patch_release in patch_releases {
        let semver::Version {
            major,
            minor,
            patch,
            ..
        } = new_patch_release
            .get_main_artifact_release()
            .expect("new patch version is semver conventional versions");

        assert!(
            re.is_match(&new_patch_release.tag_name),
            "release tag name starts with a v"
        );
        assert_eq!(latest_version.major, major, "major version is not changed");
        assert_eq!(latest_version.minor, minor, "minor version is not changed");
        assert!(latest_version.patch < patch, "patch version is bigger");
    }
}

#[tokio::test]
#[cfg_attr(not(can_reach_github_com), ignore = "github.com is not reachable")]
async fn test_fetching_new_wadm_version_from_tag_name_after_v_0_20_0() {
    let owner = "wasmCloud";
    let repo = "wadm";

    // Use 0.20.0 as the latest version, since there is a newer version
    let release_tag_name = "v0.20.0";
    // Note: starts with 'v' ─────┘
    // This is important because main release tags should always have a 'v' prefix, while other releases will not.

    let semver::Version {
        major,
        minor,
        patch,
        ..
    } = new_patch_or_pre_1_0_0_minor_version_after_version_string(
        owner,
        repo,
        release_tag_name,
        None,
    )
    .await
    .expect("Should have been able to fetch releases");

    assert_eq!(major, 0, "major version is not changed");
    assert_eq!(minor, 20, "minor version is not changed");
    assert!(patch > 1, "patch version is bigger");
}

#[tokio::test]
#[cfg_attr(not(can_reach_github_com), ignore = "github.com is not reachable")]
async fn test_fetching_external_tool_version() {
    let owner = "nats-io";
    let repo = "nats-server";
    // As of 2025-04-04, the 2.10.0 is released and 2.10.26 is the latest patch version
    let release_tag_name = "v2.10.0";

    let semver::Version {
        major,
        minor,
        patch,
        ..
    } = new_patch_or_pre_1_0_0_minor_version_after_version_string(
        owner,
        repo,
        release_tag_name,
        None,
    )
    .await
    .expect("Should have been able to fetch releases");

    assert_eq!(major, 2, "major version is not changed");
    assert_eq!(minor, 10, "minor version is not changed");
    assert!(patch > 0, "patch version is bigger");
}
