// Copyright (c) 2016-2017 Chef Software Inc. and/or applicable contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

    use std::env;
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use common::ui::UI;
    use hcore::crypto::{default_cache_key_path};
    use hcore::env as henv;
    use hcore::fs::{CACHE_ARTIFACT_PATH, CACHE_KEY_PATH, find_command};
    use hcore::os::process;
    use command::studio::enter::ARTIFACT_PATH_ENVVAR;

    use error::{Error, Result};
    use VERSION;

    const DOCKER_CMD: &'static str = "docker";
    const DOCKER_CMD_ENVVAR: &'static str = "HAB_DOCKER_BINARY";
    const DOCKER_IMAGE: &'static str = "habitat-docker-registry.bintray.io/studio";
    const DOCKER_IMAGE_ENVVAR: &'static str = "HAB_DOCKER_STUDIO_IMAGE";
    const DOCKER_OPTS_ENVVAR: &'static str = "HAB_DOCKER_OPTS";
    const DOCKER_SOCKET: &'static str = "/var/run/docker.sock";

    pub fn start_docker_studio(_ui: &mut UI, mut args: Vec<OsString>) -> Result<()> {
        let docker_cmd = find_docker_cmd()?;

        if is_image_present(&docker_cmd) {
            debug!("Found Studio Docker image locally.");
        } else {
            debug!("Failed to find Studio Docker image locally.");
            pull_image(&docker_cmd)?;
        }

        let mut volumes = vec![
            format!("{}:/src", env::current_dir().unwrap().to_string_lossy()),
            format!(
                "{}:/{}",
                default_cache_key_path(None).to_string_lossy(),
                CACHE_KEY_PATH
            ),
        ];
        if let Ok(cache_artifact_path) = henv::var(ARTIFACT_PATH_ENVVAR) {
            volumes.push(format!("{}:/{}", cache_artifact_path, CACHE_ARTIFACT_PATH));
        }
        if Path::new(DOCKER_SOCKET).exists() || cfg!(target_os = "windows") {
            volumes.push(format!("{}:{}", DOCKER_SOCKET, DOCKER_SOCKET));
        }

        let env_vars = vec![
            "HAB_DEPOT_URL",
            "HAB_DEPOT_CHANNEL",
            "HAB_ORIGIN",
            "HAB_STUDIO_SUP",
            "HAB_UPDATE_STRATEGY_FREQUENCY_MS",
            "http_proxy",
            "https_proxy",
        ];
        // We need to strip out the -D if it exists to avoid
        // it getting passed to the sup on entering the studio
        let to_cull = OsString::from("-D");
        let index = args.iter().position(|x| *x == to_cull).unwrap();
        args.remove(index);

        check_mounts(&docker_cmd, volumes.iter())?;
        run_container(docker_cmd, args, volumes.iter(), env_vars.iter())
    }


    fn find_docker_cmd() -> Result<PathBuf> {
        let docker_cmd = henv::var(DOCKER_CMD_ENVVAR).unwrap_or(DOCKER_CMD.to_string());

        match find_command(&docker_cmd) {
            Some(docker_abs_path) => Ok(docker_abs_path),
            None => Err(Error::ExecCommandNotFound(docker_cmd.into())),
        }
    }

    fn is_image_present(docker_cmd: &Path) -> bool {
        let mut cmd = Command::new(docker_cmd);
        cmd.arg("images").arg(&image_identifier()).arg("-q");
        debug!("Running command: {:?}", cmd);
        let result = cmd.output().expect("Docker command failed to spawn");

        !String::from_utf8_lossy(&result.stdout).as_ref().is_empty()
    }

    fn pull_image(docker_cmd: &Path) -> Result<()> {
        let image = image_identifier();
        let mut cmd = Command::new(docker_cmd);
        cmd.arg("pull")
            .arg(&image)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        debug!("Running command: {:?}", cmd);
        let result = cmd.spawn()
            .expect("Docker command failed to spawn")
            .wait_with_output()
            .expect("Failed to wait on child process");

        if result.status.success() {
            debug!("Docker image '{}' is present locally.", &image);
        } else {
            debug!(
                "Pulling Docker image '{}' failed with exit code: {:?}",
                &image,
                result.status
            );

            let err_output = String::from_utf8_lossy(&result.stderr);

            if err_output.contains("image") && err_output.contains("not found") {
                return Err(Error::DockerImageNotFound(image_identifier().to_string()));
            } else if err_output.contains("Cannot connect to the Docker daemon") {
                return Err(Error::DockerDaemonDown);
            } else {
                return Err(Error::DockerNetworkDown(image_identifier().to_string()));
            }
        }

        Ok(())
    }

    /// Checks whether or not the volume mounts are working.
    ///
    /// We need to ensure that filesystem sharing has been enabled, otherwise the user will be
    /// greeted with a horrible error message that's difficult to make sense of. To mitigate this,
    /// we check the studio version. This will cause Docker to go through the mounting steps, so we
    /// can watch stderr for failure, but has the advantage of not requiring a TTY.
    fn check_mounts<I, S>(docker_cmd: &Path, volumes: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut volume_args: Vec<OsString> = Vec::new();
        for vol in volumes {
            volume_args.push("--volume".into());
            volume_args.push(vol.as_ref().into());
        }

        let version_output = Command::new(docker_cmd)
            .arg("run")
            .arg("--rm")
            .arg("--privileged")
            .args(volume_args)
            .arg(image_identifier())
            .arg("-V")
            .output()
            .expect("docker failed to start");

        let stderr = String::from_utf8(version_output.stderr).unwrap();
        if !stderr.is_empty() &&
            (stderr.as_str().contains("Mounts denied") ||
                 stderr.as_str().contains("drive is not shared"))
        {
            return Err(Error::DockerFileSharingNotEnabled);
        }
        Ok(())
    }

    fn run_container<I, J, S, T>(
        docker_cmd: PathBuf,
        args: Vec<OsString>,
        volumes: I,
        env_vars: J,
    ) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        J: IntoIterator<Item = T>,
        S: AsRef<OsStr>,
        T: AsRef<str>,
    {
        let mut cmd_args: Vec<OsString> = vec![
            "run".into(),
            "--rm".into(),
            "--tty".into(),
            "--interactive".into(),
            "--privileged".into(),
        ];
        if let Ok(opts) = henv::var(DOCKER_OPTS_ENVVAR) {
            let opts = opts.split(" ")
                .map(|v| v.into())
                // Ensure we're not passing something like `--tty` again here.
                .filter(|v| !cmd_args.contains(v))
                .collect::<Vec<_>>();
            if !opts.is_empty() {
                debug!(
                    "Adding extra Docker options from {} = {:?}",
                    DOCKER_OPTS_ENVVAR,
                    opts
                );
                cmd_args.extend_from_slice(opts.as_slice());
            }
        }
        for var in env_vars {
            if let Ok(val) = henv::var(var.as_ref()) {
                debug!("Setting container env var: {:?}='{}'", var.as_ref(), val);
                cmd_args.push("--env".into());
                cmd_args.push(format!("{}={}", var.as_ref(), val).into());
            }
        }
        for vol in volumes {
            cmd_args.push("--volume".into());
            cmd_args.push(vol.as_ref().into());
        }
        cmd_args.push(image_identifier().into());
        cmd_args.extend_from_slice(args.as_slice());

        unset_proxy_env_vars();
        Ok(process::become_command(docker_cmd, cmd_args)?)
    }

    fn unset_proxy_env_vars() {
        for var in vec!["http_proxy", "https_proxy"] {
            if let Ok(_) = henv::var(var) {
                debug!("Unsetting process environment variable '{}'", var);
                env::remove_var(var);
            }
        }
    }

    /// Returns the Docker Studio image with tag for the desired version which corresponds to the
    /// same version (minus release) as this program.
    fn image_identifier() -> String {
        let version: Vec<&str> = VERSION.split("/").collect();
        henv::var(DOCKER_IMAGE_ENVVAR).unwrap_or(format!("{}:{}", DOCKER_IMAGE, version[0]))
    }
