//! `fsm-sources` — declarative manifest interpreter (TOML + `jaq` + XML + HTML + CSV +
//! `pre` + `paginate` + `[auth]`/OAuth2 + `[claims]`), protocol instance wrapper
//! (`InstanceSource`), and frozen §5.2 `SourceDispatcher`.

pub mod csv_parse;
pub mod dispatcher;
pub mod html;
pub mod instance_source;
pub mod jaq_rt;
pub mod manifest_source;
pub mod oauth2;
pub mod xml;

pub use dispatcher::SourceDispatcher;
pub use instance_source::InstanceSource;
pub use manifest_source::ManifestSource;
pub use oauth2::OAuth2TokenCache;

use fsm_core::{FsmError, Registry, SourceDeps};
use std::sync::Arc;

/// Register every embedded manifest source, valid user override (`<config_dir>/sources.d/*.toml`),
/// and every embedded protocol instance whose adapter is registered.
///
/// Registration order: call `fsm_protocols_a::register` and `fsm_protocols_b::register` BEFORE
/// `fsm_sources::register` so all protocol adapters are present for instance registration.
pub fn register(reg: &mut Registry, deps: &SourceDeps) -> Result<(), FsmError> {
    let oauth = Arc::new(OAuth2TokenCache::new());
    register_manifests_with_oauth(reg, deps, Arc::clone(&oauth))?;

    let bundle = fsm_manifests::embedded_bundle();
    for inst_file in &bundle.instances {
        let Some(adapter) = reg.protocol(&inst_file.file.protocol) else {
            return Err(FsmError::invalid(format!(
                "instance file {:?} requires unregistered protocol adapter {:?} (register protocols-a/b before fsm-sources)",
                inst_file.path, inst_file.file.protocol
            )));
        };
        for inst in &inst_file.file.instance {
            let src = InstanceSource::new(inst.clone(), Arc::clone(&adapter))?;
            reg.register(Arc::new(src))?;
        }
    }

    Ok(())
}

/// Register only embedded declarative manifests and user overrides (`sources.d/*.toml`),
/// skipping protocol instances (useful for unit tests without `fsm-protocols-{a,b}`).
pub fn register_manifests_only(reg: &mut Registry, deps: &SourceDeps) -> Result<(), FsmError> {
    let oauth = Arc::new(OAuth2TokenCache::new());
    register_manifests_with_oauth(reg, deps, oauth)
}

fn register_manifests_with_oauth(
    reg: &mut Registry,
    deps: &SourceDeps,
    oauth: Arc<OAuth2TokenCache>,
) -> Result<(), FsmError> {
    let bundle = fsm_manifests::embedded_bundle();
    for em in &bundle.manifests {
        let src = ManifestSource::new(em.manifest.clone(), deps.clone(), Arc::clone(&oauth));
        reg.register(Arc::new(src))?;
    }

    // Load user overrides from <config_dir>/sources.d/*.toml (DoS lint + S4 isolation).
    let sources_d = deps.cfg.config_dir.join("sources.d");
    let mut builtin_ids = fsm_manifests::all_builtin_ids();
    let mut builtin_envs = fsm_manifests::all_builtin_auth_envs();
    for existing in reg.all() {
        let decl = existing.decl();
        builtin_ids.insert(decl.id.clone());
        for env_name in decl.key_envs() {
            builtin_envs.insert(env_name.to_owned());
        }
    }
    let (loaded, errors) =
        fsm_manifests::user_overrides::load_user_overrides(&sources_d, &builtin_ids, &builtin_envs);

    for (path, err) in errors {
        tracing::warn!(path = %path.display(), error = %err, "ignoring invalid user manifest");
    }
    for item in loaded {
        let src = ManifestSource::new_user(item.manifest, deps.clone(), Arc::clone(&oauth));
        if let Err(err) = reg.register(Arc::new(src)) {
            tracing::warn!(
                path = %item.path.display(),
                error = %err,
                "ignoring user manifest that failed registration"
            );
        }
    }

    Ok(())
}
