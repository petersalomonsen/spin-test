use anyhow::bail;
use clap::Parser;
use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc};
use tokio::sync::oneshot::error::TryRecvError;
use wasmtime_wasi_http::{
    bindings::http::{incoming_handler::IncomingRequest, types::Scheme},
    body::HyperOutgoingBody,
    WasiHttpView,
};

mod bindings {
    wasmtime::component::bindgen!({
        world: "runner",
        path: "host-wit",
        with: {
            "wasi:io/poll": wasmtime_wasi::bindings::io::poll,
            "wasi:io/error": wasmtime_wasi::bindings::io::error,
            "wasi:io/streams": wasmtime_wasi::bindings::io::streams,
            "wasi:clocks/monotonic-clock": wasmtime_wasi::bindings::clocks::monotonic_clock,
            "wasi:http/types": wasmtime_wasi_http::bindings::http::types,
            "fermyon:spin-test/http-helper/response-receiver": super::ResponseReceiver,
        }
    });
}

const SPIN_TEST_VIRT: &[u8] = include_bytes!("../example/deps/fermyon/spin-test-virt.wasm");
const WASI_VIRT: &[u8] = include_bytes!("../example/deps/wasi/virt.wasm");
const ROUTER: &[u8] = include_bytes!("../example/deps/fermyon/router.wasm");

#[derive(clap::Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Path to test wasm
    test_path: PathBuf,
}

fn main() {
    env_logger::init();
    let cli = Cli::parse();
    let test_path = cli.test_path;
    let manifest_path =
        spin_common::paths::resolve_manifest_file_path(spin_common::paths::DEFAULT_MANIFEST_FILE)
            .unwrap();
    let raw_manifest = std::fs::read_to_string(&manifest_path).unwrap();
    let manifest = spin_manifest::manifest_from_str(&raw_manifest).unwrap();
    let app_path = match &manifest.components.first().as_ref().unwrap().1.source {
        spin_manifest::schema::v2::ComponentSource::Local(path) => path,
        spin_manifest::schema::v2::ComponentSource::Remote { .. } => {
            todo!("handle remote component sources")
        }
    };

    let test = std::fs::read(&test_path).unwrap();
    let app = std::fs::read(app_path).unwrap();
    let app = spin_componentize::componentize_if_necessary(&app)
        .unwrap()
        .into_owned();

    let encoded = encode_composition(app, test);

    let mut runtime = Runtime::new(raw_manifest, &encoded);
    let tests = vec![libtest_mimic::Trial::test(
        test_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("test"),
        move || Ok(runtime.call_run()?),
    )];
    let _ = libtest_mimic::run(&libtest_mimic::Arguments::default(), tests);
}

fn encode_composition(app: Vec<u8>, test: Vec<u8>) -> Vec<u8> {
    let composition = Composition::new();
    let virt = composition
        .instantiate("virt", SPIN_TEST_VIRT, Vec::new())
        .unwrap();
    let _wasi_virt = composition
        .instantiate("wasi_virt", WASI_VIRT, Vec::new())
        .unwrap();

    let app_args = [
        ("fermyon:spin/key-value@2.0.0", &virt),
        ("fermyon:spin/llm@2.0.0", &virt),
        ("fermyon:spin/redis@2.0.0", &virt),
        ("fermyon:spin/mysql@2.0.0", &virt),
        ("fermyon:spin/postgres@2.0.0", &virt),
        ("fermyon:spin/sqlite@2.0.0", &virt),
        ("fermyon:spin/mqtt@2.0.0", &virt),
        ("fermyon:spin/variables@2.0.0", &virt),
        ("wasi:http/outgoing-handler@0.2.0", &virt),
        // Don't stub environment yet as this messes with Python
        // ("wasi:cli/environment@0.2.0", &wasi_virt),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.export(k).unwrap().unwrap()));
    let app = composition.instantiate("app", &app, app_args).unwrap();

    let router_args = [
        ("set-component-id", &virt),
        ("wasi:http/incoming-handler@0.2.0", &app),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.export(k).unwrap().unwrap()));
    let router = composition
        .instantiate("router", ROUTER, router_args)
        .unwrap();

    let test_args = vec![
        ("wasi:http/incoming-handler@0.2.0", &router),
        ("wasi:http/outgoing-handler@0.2.0", &virt),
        ("fermyon:spin/key-value@2.0.0", &virt),
        ("fermyon:spin-test-virt/key-value-calls", &virt),
        ("fermyon:spin-test-virt/http-handler", &virt),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.export(k).unwrap().unwrap()));
    let test = composition.instantiate("test", &test, test_args).unwrap();
    let export = test.export("run").unwrap().unwrap();

    composition.export(export, "run").unwrap();
    composition.encode().unwrap()
}

struct Composition {
    graph: Rc<RefCell<wac_graph::CompositionGraph>>,
}

impl Composition {
    fn new() -> Self {
        Self {
            graph: Rc::new(RefCell::new(wac_graph::CompositionGraph::new())),
        }
    }

    pub fn instantiate<'a>(
        &self,
        name: &str,
        bytes: &[u8],
        arguments: impl IntoIterator<Item = (&'a str, Export)> + 'a,
    ) -> anyhow::Result<Instance> {
        let package =
            wac_graph::types::Package::from_bytes(name, None, Arc::new(bytes.to_owned()))?;
        let package = self.graph.borrow_mut().register_package(package)?;
        let instance = self.graph.borrow_mut().instantiate(package)?;
        for (arg_name, arg) in arguments {
            match self
                .graph
                .borrow_mut()
                .set_instantiation_argument(instance, arg_name, arg.node)
            {
                // Don't error if we try to pass an invalid argument
                Ok(_) | Err(wac_graph::Error::InvalidArgumentName { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(Instance {
            graph: self.graph.clone(),
            node: instance,
        })
    }

    fn export(&self, export: Export, name: &str) -> anyhow::Result<()> {
        Ok(self.graph.borrow_mut().export(export.node, name)?)
    }

    fn encode(&self) -> anyhow::Result<Vec<u8>> {
        Ok(self
            .graph
            .borrow_mut()
            .encode(wac_graph::EncodeOptions::default())?)
    }
}

struct Instance {
    graph: Rc<RefCell<wac_graph::CompositionGraph>>,
    node: wac_graph::NodeId,
}

impl Instance {
    /// Export a node from the instance
    ///
    /// Returns `None` if no export exists with the given name
    fn export(&self, name: &str) -> anyhow::Result<Option<Export>> {
        let node = self
            .graph
            .borrow_mut()
            .alias_instance_export(self.node, name)
            .map(Some)
            .or_else(|e| match e {
                wac_graph::Error::InstanceMissingExport { .. } => Ok(None),
                e => Err(e),
            })?;

        Ok(node.map(|node| Export { node }))
    }
}

struct Export {
    node: wac_graph::NodeId,
}

struct Runtime {
    store: wasmtime::Store<Data>,
    runner: bindings::Runner,
}

impl Runtime {
    fn new(manifest: String, composed_component: &[u8]) -> Self {
        if std::env::var("SPIN_TEST_DUMP_COMPOSITION").is_ok() {
            let _ = std::fs::write("composition.wasm", composed_component);
        }
        let engine = wasmtime::Engine::default();
        let mut store = wasmtime::Store::new(&engine, Data::new(manifest));

        let component = wasmtime::component::Component::new(&engine, composed_component).unwrap();

        let mut linker = wasmtime::component::Linker::new(&engine);
        wasmtime_wasi::command::sync::add_to_linker(&mut linker).unwrap();
        wasmtime_wasi_http::bindings::http::types::add_to_linker(&mut linker, |x| x).unwrap();
        bindings::Runner::add_to_linker(&mut linker, |x| x).unwrap();

        let (runner, _) =
            bindings::Runner::instantiate(&mut store, &component, &mut linker).unwrap();
        Self { store, runner }
    }

    fn call_run(&mut self) -> anyhow::Result<()> {
        self.runner.call_run(&mut self.store)
    }
}

/// Store specific data
struct Data {
    table: wasmtime_wasi::ResourceTable,
    ctx: wasmtime_wasi::WasiCtx,
    http_ctx: wasmtime_wasi_http::WasiHttpCtx,
    manifest: String,
}

impl Data {
    fn new(manifest: String) -> Self {
        let table = wasmtime_wasi::ResourceTable::new();
        let ctx = wasmtime_wasi::WasiCtxBuilder::new()
            .inherit_stdout()
            .inherit_stderr()
            .build();
        Self {
            table,
            ctx,
            http_ctx: wasmtime_wasi_http::WasiHttpCtx,
            manifest,
        }
    }
}

impl wasmtime_wasi_http::WasiHttpView for Data {
    fn ctx(&mut self) -> &mut wasmtime_wasi_http::WasiHttpCtx {
        &mut self.http_ctx
    }

    fn table(&mut self) -> &mut wasmtime_wasi::ResourceTable {
        &mut self.table
    }
}

impl bindings::RunnerImports for Data {
    fn get_manifest(&mut self) -> wasmtime::Result<String> {
        Ok(self.manifest.clone())
    }
}

impl bindings::fermyon::spin_test::http_helper::Host for Data {
    fn new_request(
        &mut self,
        request: wasmtime::component::Resource<wasmtime_wasi_http::types::HostOutgoingRequest>,
    ) -> wasmtime::Result<wasmtime::component::Resource<IncomingRequest>> {
        let req = self.table.get_mut(&request)?;
        use wasmtime_wasi_http::bindings::http::types::Method;
        let method = match &req.method {
            Method::Get => hyper::Method::GET,
            Method::Head => hyper::Method::HEAD,
            Method::Post => hyper::Method::POST,
            Method::Put => hyper::Method::PUT,
            Method::Delete => hyper::Method::DELETE,
            Method::Connect => hyper::Method::CONNECT,
            Method::Options => hyper::Method::OPTIONS,
            Method::Trace => hyper::Method::TRACE,
            Method::Patch => hyper::Method::PATCH,
            Method::Other(o) => hyper::Method::from_bytes(o.as_bytes())?,
        };
        let scheme = match &req.scheme {
            Some(Scheme::Http) | None => "http",
            Some(Scheme::Https) => "https",
            Some(Scheme::Other(other)) => other,
        };
        let mut builder = hyper::Request::builder().method(method).uri(format!(
            "{}://{}{}",
            scheme,
            req.authority.as_deref().unwrap_or("localhost:3000"),
            req.path_with_query.as_deref().unwrap_or("/")
        ));
        for (name, value) in req.headers.iter() {
            builder = builder.header(name, value);
        }
        let req = builder
            .body(req.body.take().unwrap_or_else(body::empty))
            .unwrap();
        self.new_incoming_request(req)
    }

    fn new_response(
        &mut self,
    ) -> wasmtime::Result<(
        wasmtime::component::Resource<wasmtime_wasi_http::types::HostResponseOutparam>,
        wasmtime::component::Resource<bindings::fermyon::spin_test::http_helper::ResponseReceiver>,
    )> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let outparam = self.new_response_outparam(tx)?;
        let receiver = self.table.push(ResponseReceiver(rx))?;
        Ok((outparam, receiver))
    }
}

impl bindings::fermyon::spin_test::http_helper::HostResponseReceiver for Data {
    fn get(
        &mut self,
        self_: wasmtime::component::Resource<ResponseReceiver>,
    ) -> wasmtime::Result<
        Option<
            wasmtime::component::Resource<
                bindings::fermyon::spin_test::http_helper::OutgoingResponse,
            >,
        >,
    > {
        let receiver = self.table.get_mut(&self_)?;
        let response = match receiver.0.try_recv() {
            Ok(r) => r?,
            Err(TryRecvError::Empty) => return Ok(None),
            Err(TryRecvError::Closed) => {
                bail!("response receiver channel closed because outparam was dropped")
            }
        };
        let (parts, body) = response.into_parts();
        let response = wasmtime_wasi_http::types::HostOutgoingResponse {
            status: parts.status,
            headers: parts.headers,
            body: Some(body),
        };
        Ok(Some(self.table.push(response)?))
    }

    fn drop(
        &mut self,
        rep: wasmtime::component::Resource<ResponseReceiver>,
    ) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

pub struct ResponseReceiver(
    tokio::sync::oneshot::Receiver<
        Result<
            hyper::Response<HyperOutgoingBody>,
            wasmtime_wasi_http::bindings::http::types::ErrorCode,
        >,
    >,
);

impl wasmtime_wasi::WasiView for Data {
    fn table(&mut self) -> &mut wasmtime_wasi::ResourceTable {
        &mut self.table
    }

    fn ctx(&mut self) -> &mut wasmtime_wasi::WasiCtx {
        &mut self.ctx
    }
}

pub mod body {
    use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
    use wasmtime_wasi_http::body::HyperIncomingBody;

    pub fn empty() -> HyperIncomingBody {
        BoxBody::new(Empty::new().map_err(|_| unreachable!()))
    }

    pub fn full(body: Vec<u8>) -> HyperIncomingBody {
        BoxBody::new(Full::new(body.into()).map_err(|_| unreachable!()))
    }
}