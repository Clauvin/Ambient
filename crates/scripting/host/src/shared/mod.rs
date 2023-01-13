use std::{
    collections::{HashMap, HashSet},
    fmt::{Display, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use anyhow::Context;
use elements_core::name;
use elements_ecs::{
    components, query, query_mut, with_component_registry, EntityData, EntityId, EntityUid,
    PrimitiveComponent, Query, QueryState, World, COMPONENT_ENTITY_ID_MIGRATERS,
};
use elements_std::asset_url::ObjectRef;
use glam::Vec3;
use indexmap::IndexMap;
use indoc::indoc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use wasi_common::WasiCtx;

use self::wasm::{GuestExports, WasmContext};

pub mod dependencies;
pub mod implementation;
pub mod wasm;

pub mod bindings;
pub mod conversion;
pub mod guest_conversion;
pub mod interface;

components!("scripting::shared", {
    script_module: ScriptModule,
    script_module_bytecode: ScriptModuleBytecode,
    script_module_compiled: (),
    script_module_errors: ScriptModuleErrors,

    // resources
    scripting_interface_name: String,
});

pub type QueryStateMap =
    slotmap::SlotMap<slotmap::DefaultKey, (Query, QueryState, Vec<PrimitiveComponent>)>;

#[derive(Default, Clone)]
pub struct EventSharedState {
    pub subscribed_events: HashSet<String>,
    pub events: Vec<(String, EntityData)>,
}

#[derive(Default, Clone)]
pub struct BaseHostGuestState {
    pub spawned_entities: HashSet<EntityUid>,
    pub event: EventSharedState,
    pub query_states: QueryStateMap,
}

pub trait GetBaseHostGuestState {
    fn base_mut(&mut self) -> &mut BaseHostGuestState;
}
impl GetBaseHostGuestState for BaseHostGuestState {
    fn base_mut(&mut self) -> &mut BaseHostGuestState {
        self
    }
}

#[derive(Clone)]
pub struct ScriptModuleBytecode(pub Vec<u8>);
impl std::fmt::Debug for ScriptModuleBytecode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ScriptModuleBytecode")
            .field(&base64::encode(&self.0))
            .finish()
    }
}
impl std::fmt::Display for ScriptModuleBytecode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ScriptModuleBytecode({} bytes)", self.0.len())
    }
}
impl Serialize for ScriptModuleBytecode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&base64::encode(&self.0))
    }
}
impl<'de> Deserialize<'de> for ScriptModuleBytecode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{self, Visitor};

        struct ScriptModuleBytecodeVisitor;
        impl<'de> Visitor<'de> for ScriptModuleBytecodeVisitor {
            type Value = ScriptModuleBytecode;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a base64-encoded string of bytes")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                base64::decode(v)
                    .map_err(|err| {
                        E::custom(format!("failed to decode base64-encoded string: {err}"))
                    })
                    .map(ScriptModuleBytecode)
            }
        }

        deserializer.deserialize_str(ScriptModuleBytecodeVisitor)
    }
}

pub fn register_entity_id_migraters() {
    COMPONENT_ENTITY_ID_MIGRATERS
        .lock()
        .push(|world, entity, old_to_new_ids| {
            if let Ok(script) = world.get_mut(entity, script_module()) {
                script.migrate_ids(old_to_new_ids);
            }
        })
}

#[derive(Debug, Clone)]
pub struct ScriptContext {
    pub event_name: String,
    pub event_data: EntityData,
    pub time: f32,
    pub frametime: f32,
}
impl ScriptContext {
    pub fn new(world: &World, event_name: &str, event_data: EntityData) -> Self {
        let time = elements_app::get_time_since_app_start(world).as_secs_f32();
        let frametime = *world.resource(elements_core::dtime());

        Self {
            event_name: event_name.to_string(),
            event_data,
            time,
            frametime,
        }
    }
}

#[derive(Default)]
pub struct ScriptModuleState<
    Bindings: Send + Sync + 'static,
    Context: WasmContext<Bindings>,
    Exports: GuestExports<Bindings, Context>,
    HostGuestState: Default,
> {
    wasm: Option<wasm::WasmState<Bindings, Context, Exports>>,
    pub shared_state: Arc<Mutex<HostGuestState>>,
    _bindings: PhantomData<Bindings>,
}

impl<
        Bindings: Send + Sync + 'static,
        Context: WasmContext<Bindings>,
        Exports: GuestExports<Bindings, Context>,
        HostGuestState: Default,
    > Clone for ScriptModuleState<Bindings, Context, Exports, HostGuestState>
{
    fn clone(&self) -> Self {
        Self {
            wasm: self.wasm.clone(),
            shared_state: self.shared_state.clone(),
            _bindings: self._bindings.clone(),
        }
    }
}
impl<
        Bindings: Send + Sync + 'static,
        Context: WasmContext<Bindings>,
        Exports: GuestExports<Bindings, Context>,
        HostGuestState: Default,
    > std::fmt::Debug for ScriptModuleState<Bindings, Context, Exports, HostGuestState>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptModuleState").finish()
    }
}
impl<
        Bindings: Send + Sync + 'static,
        Context: WasmContext<Bindings>,
        Exports: GuestExports<Bindings, Context>,
        HostGuestState: Default,
    > ScriptModuleState<Bindings, Context, Exports, HostGuestState>
{
    pub fn new(
        bytecode: &[u8],
        stdout_output: Box<dyn Fn(&World, &str) + Sync + Send>,
        stderr_output: Box<dyn Fn(&World, &str) + Sync + Send>,
        make_wasm_context: impl Fn(WasiCtx, Arc<Mutex<HostGuestState>>) -> Context,
        add_to_linker: impl Fn(&mut wasmtime::Linker<Context>) -> anyhow::Result<()>,
        interface_version: u32,
    ) -> anyhow::Result<Self> {
        let shared_state = Arc::new(Mutex::new(HostGuestState::default()));

        let wasm = if bytecode.is_empty() {
            None
        } else {
            Some(wasm::WasmState::new(
                bytecode,
                stdout_output,
                stderr_output,
                {
                    let shared_state = shared_state.clone();
                    move |wasi| make_wasm_context(wasi, shared_state.clone())
                },
                add_to_linker,
                interface_version,
            )?)
        };
        Ok(Self {
            wasm,
            shared_state,
            _bindings: PhantomData,
        })
    }

    pub fn run(&mut self, world: &mut World, context: &ScriptContext) -> anyhow::Result<()> {
        if let Some(wasm) = &mut self.wasm {
            wasm.run(world, context)?;
        }
        Ok(())
    }

    pub fn shared_state(&self) -> Arc<Mutex<HostGuestState>> {
        self.shared_state.clone()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Parameter {
    EntityUid(Option<EntityUid>),
    ObjectRef(ObjectRef),
    Integer(i32),
    Float(f32),
    Vec3(Vec3),
    String(String),
    Bool(bool),
}
impl Default for Parameter {
    fn default() -> Self {
        Parameter::Integer(0)
    }
}

pub type ParametersMap = IndexMap<String, IndexMap<String, Parameter>>;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct File {
    // TODO(mithun): consider using an enum of Plaintext(String)/Binary(Bytes) files so that people can include binary assets
    // in their crates
    pub contents: String,
    pub last_modified: chrono::DateTime<chrono::Utc>,
}
impl File {
    pub fn new_at_now(contents: String) -> Self {
        Self {
            contents,
            last_modified: chrono::Utc::now(),
        }
    }
}

pub type FileMap = HashMap<PathBuf, File>;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ScriptModule {
    files: FileMap,
    pub description: String,
    pub parameters: ParametersMap,
    last_updated_by_parameters: bool,
    pub external_component_ids: HashSet<String>,
    pub enabled: bool,
}
impl ScriptModule {
    pub fn new(
        name: &str,
        description: impl Into<String>,
        files: FileMap,
        parameters: ParametersMap,
        external_component_ids: HashSet<String>,
        enabled: bool,

        scripting_interface: &str,
    ) -> Self {
        let mut sm = ScriptModule {
            files: HashMap::new(),
            description: description.into(),
            parameters,
            enabled,
            external_component_ids,
            last_updated_by_parameters: false,
        };
        sm.files.extend(files);
        sm.populate_files(name, scripting_interface);
        sm
    }

    pub fn migrate_ids(&mut self, _old_to_new_ids: &HashMap<EntityId, EntityId>) {}

    pub fn files(&self) -> &HashMap<PathBuf, File> {
        &self.files
    }

    pub fn system_controlled_files() -> Vec<PathBuf> {
        ["src/params.rs", "src/components.rs"]
            .into_iter()
            .map(|p| p.into())
            .collect()
    }

    pub fn populate_files(&mut self, name: &str, scripting_interface: &str) {
        self.regenerate_params_file(scripting_interface);
        self.regenerate_components_file(scripting_interface);
        for (filename, contents) in Self::STATIC_FILE_TEMPLATES {
            let filename = PathBuf::from(filename);
            let contents = contents
                .replace("{{name}}", &sanitize(&name))
                .replace("{{description}}", &self.description)
                .replace("{{scripting_interface}}", scripting_interface);
            let file = File::new_at_now(contents);

            self.files.entry(filename).or_insert(file);
        }
        self.last_updated_by_parameters = false;
    }

    pub fn update_parameters(&mut self, parameters: ParametersMap, scripting_interface: &str) {
        self.parameters = parameters;
        self.last_updated_by_parameters = true;
        self.regenerate_params_file(scripting_interface);
        self.regenerate_components_file(scripting_interface);
    }

    pub fn insert(
        &mut self,
        scripting_interfaces: &[&str],
        relative_path: PathBuf,
        new_file: String,
    ) -> anyhow::Result<()> {
        let relative_path = elements_std::path::normalize(&relative_path);
        if ScriptModule::system_controlled_files().contains(&relative_path) {
            anyhow::bail!("{relative_path:?} is system-controlled and cannot be updated");
        }

        if relative_path == Path::new("Cargo.toml") {
            self.files.insert(
                relative_path,
                File::new_at_now(dependencies::merge_cargo_toml(
                    scripting_interfaces,
                    &self
                        .files
                        .get(Path::new("Cargo.toml"))
                        .context("no Cargo.toml")?
                        .contents,
                    &new_file,
                )?),
            );
        } else {
            self.files.insert(relative_path, File::new_at_now(new_file));
        }

        Ok(())
    }

    pub fn remove(&mut self, relative_path: &Path) {
        let relative_path = elements_std::path::normalize(relative_path);
        if ScriptModule::system_controlled_files()
            .iter()
            .any(|pb| pb == &relative_path)
        {
            return;
        }
        self.files.remove(&relative_path);
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn last_updated_by_parameters(&self) -> bool {
        self.last_updated_by_parameters
    }
}
impl ScriptModule {
    const STATIC_FILE_TEMPLATES: &[(&'static str, &'static str)] = &[
        (
            "Cargo.toml",
            indoc! {r#"
                [package]
                edition = "2021"
                name = "{{name}}"
                description = "{{description}}"
                version = "0.1.0"

                [lib]
                crate-type = ["cdylib"]

                [dependencies]
                {{scripting_interface}} = {path = "../../interfaces/{{scripting_interface}}"}
            "#},
        ),
        (
            "src/lib.rs",
            indoc! {r#"
                use {{scripting_interface}}::*;
                pub mod params;
                pub mod components;

                pub async fn main() -> EventResult {
                    EventOk
                }
            "#},
        ),
    ];

    fn regenerate_params_file(&mut self, scripting_interface: &str) {
        let mut contents = String::new();
        let _ = writeln!(contents, "#![allow(unused_imports)]");
        for (category, parameters) in &self.parameters {
            let category = category.trim().replace(' ', "_").to_lowercase();
            if category.is_empty() {
                continue;
            }

            let _ = writeln!(contents, "pub mod {category} {{");
            let _ = writeln!(contents, "    use {}::*;", scripting_interface);
            for (key, value) in parameters {
                let key = key.trim().replace(' ', "_").to_uppercase();
                if key.is_empty() {
                    continue;
                }
                let value = match value {
                    Parameter::EntityUid(Some(uid)) => {
                        format!("EntityUid = EntityUid::new(\"{uid}\")")
                    }
                    Parameter::EntityUid(None) => continue,
                    Parameter::ObjectRef(url) => {
                        format!(r#"ObjectRef = ObjectRef::new("{url}")"#)
                    }
                    Parameter::Integer(v) => format!("i32 = {v}"),
                    Parameter::Float(v) => format!("f32 = {v} as f32"),
                    Parameter::Vec3(v) => {
                        format!(
                            "Vec3 = vec3({} as f32, {} as f32, {} as f32)",
                            v.x, v.y, v.z
                        )
                    }
                    Parameter::String(v) => format!(r#"&str = {v:?}"#),
                    Parameter::Bool(v) => format!("bool = {v}"),
                };

                let _ = writeln!(contents, "    pub const {key}: {value};");
            }
            let _ = writeln!(contents, "}}");
        }

        self.files
            .insert("src/params.rs".into(), File::new_at_now(contents));
    }

    fn regenerate_components_file(&mut self, scripting_interface: &str) {
        enum ComponentTreeNode {
            Category(HashMap<String, ComponentTreeNode>),
            Component { typename: &'static str, id: String },
        }
        impl Default for ComponentTreeNode {
            fn default() -> Self {
                ComponentTreeNode::Category(Default::default())
            }
        }
        impl ComponentTreeNode {
            fn insert(&mut self, id_portion: &str, id: &str, typename: &'static str) {
                if let ComponentTreeNode::Category(hm) = self {
                    let (prefix, suffix) = id_portion.split_once("::").unwrap_or(("", id_portion));
                    if prefix.is_empty() {
                        hm.insert(
                            suffix.to_string(),
                            ComponentTreeNode::Component {
                                typename,
                                id: id.to_string(),
                            },
                        );
                    } else {
                        hm.entry(prefix.to_string())
                            .or_default()
                            .insert(suffix, id, typename);
                    }
                }
            }
        }

        let supported_types: HashMap<_, _> = bindings::SUPPORTED_COMPONENT_TYPES
            .iter()
            .copied()
            .collect();

        let mut root = ComponentTreeNode::default();
        with_component_registry(|registry| {
            for component in registry.all_external() {
                if let Some(typename) = supported_types.get(&component.type_id()) {
                    root.insert(&component.get_id(), &component.get_id(), typename);
                }
            }
        });

        fn write_to_file(
            output: &mut String,
            name: &str,
            component: &ComponentTreeNode,
            depth: usize,
            scripting_interface: &str,
        ) {
            let space = " ".repeat(depth * 4);
            match component {
                ComponentTreeNode::Category(hm) => {
                    if name.is_empty() {
                        for (key, value) in hm {
                            write_to_file(output, key, value, 0, scripting_interface);
                        }
                    } else {
                        writeln!(output, "{space}pub mod {name} {{").ok();
                        writeln!(output, "{space}    use {}::*;", scripting_interface).ok();
                        for (key, value) in hm {
                            write_to_file(output, key, value, depth + 1, scripting_interface);
                        }
                        writeln!(output, "{space}}}").ok();
                    }
                }
                ComponentTreeNode::Component { typename, id, .. } => {
                    writeln!(
                        output,
                        r#"{space}static {}: LazyComponent<{typename}> = lazy_component!("{id}");"#,
                        name.to_uppercase()
                    )
                    .ok();
                    writeln!(
                        output,
                        "{space}pub fn {name}() -> Component<{typename}> {{ *{} }}",
                        name.to_uppercase()
                    )
                    .ok();
                }
            }
        }
        let mut contents = String::new();
        let _ = writeln!(contents, "#![allow(unused_imports)]");
        write_to_file(&mut contents, "", &root, 0, scripting_interface);

        self.files
            .insert("src/components.rs".into(), File::new_at_now(contents));
    }
}

impl Display for ScriptModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ScriptModule")
    }
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct ScriptModuleErrors {
    pub compiletime: Vec<String>,
    pub runtime: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ScriptModuleBundle {
    pub name: String,
    pub files: FileMap,
    pub description: String,
    pub parameters: ParametersMap,
    #[serde(default)]
    pub external_component_ids: HashSet<String>,
}
impl ScriptModuleBundle {
    pub fn to_json(name: &str, sm: &ScriptModule) -> String {
        let mut files = sm.files().clone();
        for path in ScriptModule::system_controlled_files() {
            files.remove(&path);
        }
        serde_json::to_string_pretty(&ScriptModuleBundle {
            name: name.to_owned(),
            files,
            description: sm.description.clone(),
            parameters: sm.parameters.clone(),
            external_component_ids: sm.external_component_ids.clone(),
        })
        .unwrap()
    }
}

pub fn write_files_to_directory(
    base_path: &Path,
    files: &[(PathBuf, String)],
) -> anyhow::Result<()> {
    let folders: HashSet<_> = files
        .iter()
        .map(|(p, _)| p)
        .filter_map(|k| k.parent().map(|p| p.to_owned()))
        .collect();
    for folder in folders {
        std::fs::create_dir_all(base_path.join(folder))?;
    }

    for (path, contents) in files {
        std::fs::write(base_path.join(path), contents)?;
    }
    Ok(())
}

pub fn all_module_names_sanitized(world: &World, include_disabled_modules: bool) -> Vec<String> {
    query(script_module())
        .iter(world, None)
        .filter_map(|(id, sm)| {
            (include_disabled_modules || sm.enabled).then(|| sanitize(&get_module_name(world, id)))
        })
        .collect()
}

pub fn write_workspace_files(
    workspace_path: &Path,
    script_module_sanitized_names: &[String],
    write_workspace_toml: bool,
) {
    let vscode_dir = workspace_path.join(".vscode");
    let mut workspace_files = vec![
        (
            workspace_path.join("rust-toolchain.toml"),
            indoc! {r#"
            [toolchain]
            targets = ["wasm32-wasi"]
            "#}
            .into(),
        ),
        (
            workspace_path.join(".cargo").join("config.toml"),
            indoc! {r#"
            [build]
            target = "wasm32-wasi"
            "#}
            .into(),
        ),
        (
            vscode_dir.join("extensions.json"),
            r#"{"recommendations": ["rust-lang.rust-analyzer"]}"#.into(),
        ),
        (
            vscode_dir.join("settings.json"),
            indoc! {r#"
            {
                "rust-analyzer.cargo.target": "wasm32-wasi"
            }
            "#}
            .into(),
        ),
    ];
    if write_workspace_toml {
        workspace_files.push((
            workspace_path.join("Cargo.toml"),
            format!("[workspace]\nmembers = {script_module_sanitized_names:?}"),
        ));
    }

    for (path, contents) in workspace_files {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }
}

pub fn remove_old_script_modules(scripts_dir: &Path, script_module_sanitized_names: &[String]) {
    // Remove all directories that are not within the current working set of modules.
    if let Ok(entries) = std::fs::read_dir(scripts_dir) {
        for path in entries
            .filter_map(Result::ok)
            .map(|de| de.path())
            .filter(|p| p.is_dir())
            .filter(|p| {
                let dir_name = p.file_name().unwrap_or_default().to_string_lossy();
                let should_be_kept = dir_name == "target"
                    || dir_name.starts_with('.')
                    || script_module_sanitized_names
                        .iter()
                        .any(|m| m.as_str() == dir_name);
                !should_be_kept
            })
        {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

pub fn sanitize<T: Display>(val: &T) -> String {
    val.to_string().replace(':', "_")
}

pub fn unsanitize<T: FromStr>(val: &str) -> anyhow::Result<T>
where
    <T as FromStr>::Err: std::error::Error + Send + Sync + 'static,
{
    Ok(val.replace('_', ":").parse()?)
}

pub fn get_module_entity_id(world: &World, target: &str) -> Option<EntityId> {
    let target = target.split('-').last().unwrap_or(target);

    if let Ok(id) = unsanitize::<EntityId>(target) {
        return Some(id);
    }

    query(name())
        .incl(script_module())
        .iter(world, None)
        .find(|(_, name)| name.as_str() == target)
        .map(|(id, _)| id)
}

pub fn get_module_name(world: &World, id: EntityId) -> String {
    world.get_cloned(id, name()).unwrap_or(id.to_string())
}

pub fn update_components(world: &mut World) {
    let scripting_interface_name = world.resource(scripting_interface_name()).clone();
    for (_, sm, name) in query_mut(script_module(), name()).iter(world, None) {
        sm.populate_files(name, &scripting_interface_name);
    }
}
