/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::pin;
use tokio::process::Command;

use crate::async_pipe::{get_socket_name, get_socket_rw_stream, AsyncPipe};
use crate::constants::VSCODE_CLI_QUALITY;
use crate::download_cache::DownloadCache;
use crate::log;
use crate::options::Quality;
use crate::update_service::{
	unzip_downloaded_release, Platform, Release, TargetKind, UpdateService,
};
use crate::util::errors::AnyError;
use crate::util::http::{self, ReqwestSimpleHttp};
use crate::util::io::SilentCopyProgress;
use crate::util::sync::{new_barrier, Barrier, BarrierOpener};
use crate::{
	tunnels::legal,
	util::{errors::CodeError, prereqs::PreReqChecker},
};

use super::{args::ServeWebArgs, CommandContext};

/// Length of a commit hash, for validation
const COMMIT_HASH_LEN: usize = 40;
/// Number of seconds where, if there's no connections to a VS Code server,
/// the server is shut down.
const SERVER_IDLE_TIMEOUT_SECS: u64 = 60 * 60;
/// Number of seconds in which the server times out when there is a connection
/// (should be large enough to basically never happen)
const SERVER_ACTIVE_TIMEOUT_SECS: u64 = SERVER_IDLE_TIMEOUT_SECS * 24 * 30 * 12;
/// How long to cache the "latest" version we get from the update service.
const RELEASE_CACHE_SECS: u64 = 60 * 60;

/// Implements the vscode "server of servers". Clients who go to the URI get
/// served the latest version of the VS Code server whenever they load the
/// page. The VS Code server prefixes all assets and connections it loads with
/// its version string, so existing clients can continue to get served even
/// while new clients get new VS Code Server versions.
pub async fn serve_web(ctx: CommandContext, mut args: ServeWebArgs) -> Result<i32, AnyError> {
	legal::require_consent(&ctx.paths, args.accept_server_license_terms)?;
	let mut addr: SocketAddr = match &args.host {
		Some(h) => h.parse().map_err(CodeError::InvalidHostAddress)?,
		None => SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
	};
	addr.set_port(args.port);

	let platform: crate::update_service::Platform = PreReqChecker::new().verify().await?;

	if !args.without_connection_token {
		// Ensure there's a defined connection token, since if multiple server versions
		// are excuted, they will need to have a single shared token.
		let connection_token = args
			.connection_token
			.clone()
			.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
		ctx.log.result(format!(
			"Web UI available at http://{}?tkn={}",
			addr, connection_token,
		));
		args.connection_token = Some(connection_token);
	} else {
		ctx.log
			.result(format!("Web UI available at http://{}", addr));
		args.connection_token = None;
	}

	let cm = ConnectionManager::new(&ctx, platform, args);
	let make_svc = make_service_fn(move |_conn| {
		let cm = cm.clone();
		let log = ctx.log.clone();
		let service = service_fn(move |req| handle(cm.clone(), log.clone(), req));
		async move { Ok::<_, Infallible>(service) }
	});

	let server = Server::bind(&addr).serve(make_svc);

	server.await.map_err(CodeError::CouldNotListenOnInterface)?;

	Ok(0)
}

/// Handler function for an inbound request
async fn handle(
	cm: Arc<ConnectionManager>,
	log: log::Logger,
	req: Request<Body>,
) -> Result<Response<Body>, Infallible> {
	let release = if let Some((r, _)) = get_release_from_path(req.uri().path(), cm.platform) {
		r
	} else {
		match cm.get_latest_release().await {
			Ok(r) => r,
			Err(e) => {
				error!(log, "error getting latest version: {}", e);
				return Ok(response::code_err(e));
			}
		}
	};

	Ok(match cm.get_connection(release).await {
		Ok(rw) => {
			if req.headers().contains_key(hyper::header::UPGRADE) {
				forward_ws_req_to_server(cm.log.clone(), rw, req).await
			} else {
				forward_http_req_to_server(rw, req).await
			}
		}
		Err(CodeError::ServerNotYetDownloaded) => response::wait_for_download(),
		Err(e) => response::code_err(e),
	})
}

/// Gets the release info from the VS Code path prefix, which is in the
/// format `/<quality>-<commit>/...`
fn get_release_from_path(path: &str, platform: Platform) -> Option<(Release, String)> {
	if !path.starts_with('/') {
		return None; // paths must start with '/'
	}

	let path = &path[1..];
	let i = path.find('/').unwrap_or(path.len());
	let quality_commit_sep = path.get(..i).and_then(|p| p.find('-'))?;

	let (quality_commit, remaining) = path.split_at(i);
	let (quality, commit) = quality_commit.split_at(quality_commit_sep);

	if !is_commit_hash(commit) {
		return None;
	}

	Some((
		Release {
			// remember to trim off the leading '/' which is now part of th quality
			quality: Quality::try_from(quality).ok()?,
			commit: commit.to_string(),
			platform,
			target: TargetKind::Web,
			name: "".to_string(),
		},
		remaining.to_string(),
	))
}

/// Proxies the standard HTTP request to the async pipe, returning the piped response
async fn forward_http_req_to_server(
	(rw, handle): (AsyncPipe, ConnectionHandle),
	req: Request<Body>,
) -> Response<Body> {
	let (mut request_sender, connection) =
		match hyper::client::conn::Builder::new().handshake(rw).await {
			Ok(r) => r,
			Err(e) => return response::connection_err(e),
		};

	tokio::spawn(connection);

	let res = request_sender
		.send_request(req)
		.await
		.unwrap_or_else(response::connection_err);

	// technically, we should buffer the body into memory since it may not be
	// read at this point, but because the keepalive time is very large
	// there's not going to be responses that take hours to send and x
	// cause us to kill the server before the response is sent
	drop(handle);

	res
}

/// Proxies the websocket request to the async pipe
async fn forward_ws_req_to_server(
	log: log::Logger,
	(rw, handle): (AsyncPipe, ConnectionHandle),
	mut req: Request<Body>,
) -> Response<Body> {
	// splicing of client and servers inspired by https://github.com/hyperium/hyper/blob/fece9f7f50431cf9533cfe7106b53a77b48db699/examples/upgrades.rs
	let (mut request_sender, connection) =
		match hyper::client::conn::Builder::new().handshake(rw).await {
			Ok(r) => r,
			Err(e) => return response::connection_err(e),
		};

	tokio::spawn(connection);

	let mut proxied_req = Request::builder().uri(req.uri());
	for (k, v) in req.headers() {
		proxied_req = proxied_req.header(k, v);
	}

	let mut res = request_sender
		.send_request(proxied_req.body(Body::empty()).unwrap())
		.await
		.unwrap_or_else(response::connection_err);

	let mut proxied_res = Response::new(Body::empty());
	*proxied_res.status_mut() = res.status();
	for (k, v) in res.headers() {
		proxied_res.headers_mut().insert(k, v.clone());
	}

	// only start upgrade at this point in case the server decides to deny socket
	if res.status() == hyper::StatusCode::SWITCHING_PROTOCOLS {
		tokio::spawn(async move {
			let (s_req, s_res) =
				tokio::join!(hyper::upgrade::on(&mut req), hyper::upgrade::on(&mut res));

			match (s_req, s_res) {
				(Err(e1), Err(e2)) => debug!(
					log,
					"client ({}) and server ({}) websocket upgrade failed", e1, e2
				),
				(Err(e1), _) => debug!(log, "client ({}) websocket upgrade failed", e1),
				(_, Err(e2)) => debug!(log, "server ({}) websocket upgrade failed", e2),
				(Ok(mut s_req), Ok(mut s_res)) => {
					trace!(log, "websocket upgrade succeeded");
					let r = tokio::io::copy_bidirectional(&mut s_req, &mut s_res).await;
					trace!(log, "websocket closed (error: {:?})", r.err());
				}
			}

			drop(handle);
		});
	}

	proxied_res
}

/// Returns whether the string looks like a commit hash.
fn is_commit_hash(s: &str) -> bool {
	s.len() == COMMIT_HASH_LEN && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Module holding original responses the CLI's server makes.
mod response {
	use const_format::concatcp;

	use crate::constants::QUALITYLESS_SERVER_NAME;

	use super::*;

	pub fn connection_err(err: hyper::Error) -> Response<Body> {
		Response::builder()
			.status(503)
			.body(Body::from(format!("Error connecting to server: {:?}", err)))
			.unwrap()
	}

	pub fn code_err(err: CodeError) -> Response<Body> {
		Response::builder()
			.status(500)
			.body(Body::from(format!("Error serving request: {}", err)))
			.unwrap()
	}

	pub fn wait_for_download() -> Response<Body> {
		Response::builder()
			.status(202)
			.header("Content-Type", "text/html") // todo: get latest
			.body(Body::from(concatcp!("The latest version of the ", QUALITYLESS_SERVER_NAME, " is downloading, please wait a moment...<script>setTimeout(()=>location.reload(),1500)</script>", )))
			.unwrap()
	}
}

/// Handle returned when getting a stream to the server, used to refcount
/// connections to a server so it can be disposed when there are no more clients.
struct ConnectionHandle {
	client_counter: Arc<tokio::sync::watch::Sender<usize>>,
}

impl ConnectionHandle {
	pub fn new(client_counter: Arc<tokio::sync::watch::Sender<usize>>) -> Self {
		client_counter.send_modify(|v| {
			*v += 1;
		});
		Self { client_counter }
	}
}

impl Drop for ConnectionHandle {
	fn drop(&mut self) {
		self.client_counter.send_modify(|v| {
			*v -= 1;
		});
	}
}

type StartData = (PathBuf, Arc<tokio::sync::watch::Sender<usize>>);

/// State stored in the ConnectionManager for each server version.
struct VersionState {
	downloaded: bool,
	socket_path: Barrier<Result<StartData, String>>,
}

type ConnectionStateMap = Arc<Mutex<HashMap<(Quality, String), VersionState>>>;

/// Manages the connections to running web UI instances. Multiple web servers
/// can run concurrently, with routing based on the URL path.
struct ConnectionManager {
	pub platform: Platform,
	pub log: log::Logger,
	args: ServeWebArgs,
	/// Cache where servers are stored
	cache: DownloadCache,
	/// Mapping of (Quality, Commit) to the state each server is in
	state: ConnectionStateMap,
	/// Update service instance
	update_service: UpdateService,
	/// Cache of the latest released version, storing the time we checked as well
	latest_version: tokio::sync::Mutex<Option<(Instant, Release)>>,
}

fn key_for_release(release: &Release) -> (Quality, String) {
	(release.quality, release.commit.clone())
}

impl ConnectionManager {
	pub fn new(ctx: &CommandContext, platform: Platform, args: ServeWebArgs) -> Arc<Self> {
		Arc::new(Self {
			platform,
			args,
			log: ctx.log.clone(),
			cache: DownloadCache::new(ctx.paths.web_server_storage()),
			update_service: UpdateService::new(
				ctx.log.clone(),
				Arc::new(ReqwestSimpleHttp::with_client(ctx.http.clone())),
			),
			state: ConnectionStateMap::default(),
			latest_version: tokio::sync::Mutex::default(),
		})
	}

	/// Gets a connection to a server version
	pub async fn get_connection(
		&self,
		release: Release,
	) -> Result<(AsyncPipe, ConnectionHandle), CodeError> {
		// todo@connor4312: there is likely some performance benefit to
		// implementing a 'keepalive' for these connections.
		let (path, counter) = self.get_version_data(release).await?;
		let handle = ConnectionHandle::new(counter);
		let rw = get_socket_rw_stream(&path).await?;
		Ok((rw, handle))
	}

	/// Gets the latest release for the CLI quality, caching its result for some
	/// time to allow for fast loads.
	pub async fn get_latest_release(&self) -> Result<Release, CodeError> {
		let mut latest = self.latest_version.lock().await;
		let now = Instant::now();
		if let Some((checked_at, release)) = &*latest {
			if checked_at.elapsed() < Duration::from_secs(RELEASE_CACHE_SECS) {
				return Ok(release.clone());
			}
		}

		let quality = VSCODE_CLI_QUALITY
			.ok_or_else(|| CodeError::UpdatesNotConfigured("no configured quality"))
			.and_then(|q| {
				Quality::try_from(q).map_err(|_| CodeError::UpdatesNotConfigured("unknown quality"))
			})?;

		let release = self
			.update_service
			.get_latest_commit(self.platform, TargetKind::Web, quality)
			.await
			.map_err(|e| CodeError::UpdateCheckFailed(e.to_string()));

		// If the update service is unavailable and we have stale data, use that
		if let (Err(e), Some((_, previous))) = (&release, &*latest) {
			warning!(self.log, "error getting latest release, using stale: {}", e);
			return Ok(previous.clone());
		}

		let release = release?;
		debug!(self.log, "refreshed latest release: {}", release);
		*latest = Some((now, release.clone()));

		Ok(release)
	}

	/// Gets the StartData for the a version of the VS Code server, triggering
	/// download/start if necessary. It returns `CodeError::ServerNotYetDownloaded`
	/// while the server is downloading, which is used to have a refresh loop on the page.
	async fn get_version_data(&self, release: Release) -> Result<StartData, CodeError> {
		self.get_version_data_inner(release)?
			.wait()
			.await
			.unwrap()
			.map_err(CodeError::ServerDownloadError)
	}

	fn get_version_data_inner(
		&self,
		release: Release,
	) -> Result<Barrier<Result<StartData, String>>, CodeError> {
		let mut state = self.state.lock().unwrap();
		let key = key_for_release(&release);
		if let Some(s) = state.get_mut(&key) {
			if !s.downloaded {
				if s.socket_path.is_open() {
					s.downloaded = true;
				} else {
					return Err(CodeError::ServerNotYetDownloaded);
				}
			}

			return Ok(s.socket_path.clone());
		}

		let (socket_path, opener) = new_barrier();
		let state_map_dup = self.state.clone();
		let args = StartArgs {
			args: self.args.clone(),
			log: self.log.clone(),
			opener,
			release,
		};

		if let Some(p) = self.cache.exists(&args.release.commit) {
			state.insert(
				key.clone(),
				VersionState {
					socket_path: socket_path.clone(),
					downloaded: true,
				},
			);

			tokio::spawn(async move {
				Self::start_version(args, p).await;
				state_map_dup.lock().unwrap().remove(&key);
			});
			Ok(socket_path)
		} else {
			state.insert(
				key.clone(),
				VersionState {
					socket_path,
					downloaded: false,
				},
			);
			let update_service = self.update_service.clone();
			let cache = self.cache.clone();
			tokio::spawn(async move {
				Self::download_version(args, update_service.clone(), cache.clone()).await;
				state_map_dup.lock().unwrap().remove(&key);
			});
			Err(CodeError::ServerNotYetDownloaded)
		}
	}

	/// Downloads a server version into the cache and starts it.
	async fn download_version(
		args: StartArgs,
		update_service: UpdateService,
		cache: DownloadCache,
	) {
		let release_for_fut = args.release.clone();
		let log_for_fut = args.log.clone();
		let dir_fut = cache.create(&args.release.commit, |target_dir| async move {
			info!(log_for_fut, "Downloading server {}", release_for_fut.commit);
			let tmpdir = tempfile::tempdir().unwrap();
			let response = update_service.get_download_stream(&release_for_fut).await?;

			let name = response.url_path_basename().unwrap();
			let archive_path = tmpdir.path().join(name);
			http::download_into_file(
				&archive_path,
				log_for_fut.get_download_logger("Downloading server:"),
				response,
			)
			.await?;
			unzip_downloaded_release(&archive_path, &target_dir, SilentCopyProgress())?;
			Ok(())
		});

		match dir_fut.await {
			Err(e) => args.opener.open(Err(e.to_string())),
			Ok(dir) => Self::start_version(args, dir).await,
		}
	}

	/// Starts a downloaded server that can be found in the given `path`.
	async fn start_version(args: StartArgs, path: PathBuf) {
		info!(args.log, "Starting server {}", args.release.commit);

		let executable = path
			.join("bin")
			.join(args.release.quality.server_entrypoint());
		let socket_path = get_socket_name();

		#[cfg(not(windows))]
		let mut cmd = Command::new(&executable);
		#[cfg(windows)]
		let mut cmd = {
			let mut cmd = Command::new("cmd");
			cmd.arg("/Q");
			cmd.arg("/C");
			cmd.arg(&executable);
			cmd
		};

		cmd.stdin(std::process::Stdio::null());
		cmd.stderr(std::process::Stdio::piped());
		cmd.stdout(std::process::Stdio::piped());
		cmd.arg("--socket-path");
		cmd.arg(&socket_path);

		// License agreement already checked by the `server_web` function.
		cmd.args(["--accept-server-license-terms"]);

		if let Some(a) = &args.args.server_data_dir {
			cmd.arg("--server-data-dir");
			cmd.arg(a);
		}
		if let Some(a) = &args.args.user_data_dir {
			cmd.arg("--user-data-dir");
			cmd.arg(a);
		}
		if let Some(a) = &args.args.extensions_dir {
			cmd.arg("--extensions-dir");
			cmd.arg(a);
		}
		if args.args.without_connection_token {
			cmd.arg("--without-connection-token");
		}
		if let Some(ct) = &args.args.connection_token {
			cmd.arg("--connection-token");
			cmd.arg(ct);
		}

		// removed, otherwise the workbench will not be usable when running the CLI from sources.
		cmd.env_remove("VSCODE_DEV");

		let mut child = match cmd.spawn() {
			Ok(c) => c,
			Err(e) => {
				args.opener.open(Err(e.to_string()));
				return;
			}
		};

		let (mut stdout, mut stderr) = (
			BufReader::new(child.stdout.take().unwrap()).lines(),
			BufReader::new(child.stderr.take().unwrap()).lines(),
		);

		// wrapped option to prove that we only use this once in the loop
		let (counter_tx, mut counter_rx) = tokio::sync::watch::channel(0);
		let mut opener = Some((args.opener, socket_path, Arc::new(counter_tx)));
		let commit_prefix = &args.release.commit[..7];
		let kill_timer = tokio::time::sleep(Duration::from_secs(SERVER_IDLE_TIMEOUT_SECS));
		pin!(kill_timer);

		loop {
			tokio::select! {
				Ok(Some(l)) = stdout.next_line() => {
					info!(args.log, "[{} stdout]: {}", commit_prefix, l);

					if l.contains("Server bound to") {
						if let Some((opener, path, counter_tx)) = opener.take() {
							opener.open(Ok((path, counter_tx)));
						}
					}
				}
				Ok(Some(l)) = stderr.next_line() => {
					info!(args.log, "[{} stderr]: {}", commit_prefix, l);
				},
				n = counter_rx.changed() => {
					kill_timer.as_mut().reset(match n {
						// err means that the record was dropped
						Err(_) => tokio::time::Instant::now(),
						Ok(_) => {
							if *counter_rx.borrow() == 0 {
								tokio::time::Instant::now() + Duration::from_secs(SERVER_IDLE_TIMEOUT_SECS)
							} else {
								tokio::time::Instant::now() + Duration::from_secs(SERVER_ACTIVE_TIMEOUT_SECS)
							}
						}
					});
				}
				_ = &mut kill_timer => {
					info!(args.log, "[{} process]: idle timeout reached, ending", commit_prefix);
					let _ = child.kill().await;
					break;
				}
				e = child.wait() => {
					info!(args.log, "[{} process]: exited: {:?}", commit_prefix, e);
					break;
				}
			}
		}
	}
}

struct StartArgs {
	log: log::Logger,
	args: ServeWebArgs,
	release: Release,
	opener: BarrierOpener<Result<StartData, String>>,
}