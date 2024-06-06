use anyhow::anyhow;
use anyhow::Context as _;
use bindings::{exports::wasi::http::types::HeaderError, VirtualizedApp};

mod bindings {
    wasmtime::component::bindgen!({
        world: "virtualized-app",
        path: "../host-wit",
        with: {
            "wasi:io": wasmtime_wasi::bindings::io,
            "wasi:clocks": wasmtime_wasi::bindings::clocks,
        }
    });
}

fn main() -> anyhow::Result<()> {
    let tests_dir = conformance_tests::download_tests()?;

    for test in conformance_tests::tests(&tests_dir)? {
        let engine = wasmtime::Engine::default();
        let manifest = String::from_utf8(std::fs::read(test.manifest)?)?;
        let mut store = wasmtime::Store::new(&engine, StoreData::new(manifest));
        let mut linker = wasmtime::component::Linker::new(&engine);
        let component = spin_test::Component::from_file(test.component)?;
        let component = spin_test::virtualize_app(component).context("failed to virtualize app")?;

        let component = wasmtime::component::Component::new(&engine, component)?;
        wasmtime_wasi::add_to_linker_sync(&mut linker)?;
        bindings::VirtualizedApp::add_to_linker(&mut linker, |x| x)?;

        let (instance, _) = bindings::VirtualizedApp::instantiate(&mut store, &component, &linker)?;
        for invocation in test.config.invocations {
            let conformance_tests::config::Invocation::Http(invocation) = invocation;
            let fields = Fields::new(&instance, &mut store)?;
            for header in invocation.request.headers.iter() {
                fields.append(&mut store, &header.name, &header.value.clone().into_bytes())??;
            }
            let outgoing_request = OutgoingRequest::new(&instance, &mut store, fields)?;
            outgoing_request
                .set_path_with_query(&mut store, Some(invocation.request.path.as_str()))?
                .map_err(|_| anyhow!("invalid request path"))?;
            let request = IncomingRequest::new(&instance, &mut store, outgoing_request)?;
            let (out, rx) = new_response(&instance, &mut store)?;
            instance
                .wasi_http_incoming_handler()
                .call_handle(&mut store, request.resource, out)?;
            let response = rx.get(&mut store)?.context("no response found")?;

            let status = response.status(&mut store)?;
            let body = response
                .consume(&mut store)?
                .map_err(|_| anyhow!("response body already consumed"))?
                .stream(&mut store)?
                .map_err(|_| anyhow!("response body stream already consumed"))?
                .blocking_read(&mut store, u64::MAX)??;
            let body = String::from_utf8(body).unwrap_or_else(|_| String::from("invalid utf-8"));
            assert_eq!(
                status, invocation.response.status,
                "request to Spin failed\nbody:\n{body}",
            );

            let mut actual_headers = response
                .headers(&mut store)?
                .entries(&mut store)?
                .into_iter()
                .map(|(k, v)| {
                    (
                        k.to_lowercase(),
                        String::from_utf8(v).unwrap().to_lowercase(),
                    )
                })
                .collect::<std::collections::HashMap<_, _>>();
            for expected_header in invocation.response.headers {
                let expected_name = expected_header.name.to_lowercase();
                let expected_value = expected_header.value.map(|v| v.to_lowercase());
                let actual_value = actual_headers.remove(&expected_name);
                let Some(actual_value) = actual_value.as_deref() else {
                    if expected_header.optional {
                        continue;
                    } else {
                        panic!(
                            "expected header {name} not found in response",
                            name = expected_header.name
                        )
                    }
                };
                if let Some(expected_value) = expected_value {
                    assert_eq!(actual_value, expected_value);
                }
            }
            if !actual_headers.is_empty() {
                panic!("unexpected headers: {actual_headers:?}");
            }

            if let Some(expected_body) = invocation.response.body {
                assert_eq!(body, expected_body);
            }
        }
    }
    Ok(())
}

struct Fields<'a> {
    guest: bindings::exports::wasi::http::types::GuestFields<'a>,
    resource: wasmtime::component::ResourceAny,
}

impl<'a> Fields<'a> {
    pub fn new<T>(
        instance: &'a VirtualizedApp,
        store: &mut wasmtime::Store<T>,
    ) -> anyhow::Result<Self> {
        let guest = instance.wasi_http_types().fields();
        let resource = guest.call_constructor(store)?;
        Ok(Self { guest, resource })
    }

    pub fn append<T>(
        &self,
        store: &mut wasmtime::Store<T>,
        name: &String,
        value: &Vec<u8>,
    ) -> anyhow::Result<Result<(), HeaderError>> {
        self.guest.call_append(store, self.resource, name, value)
    }

    fn entries(
        &self,
        store: &mut wasmtime::Store<StoreData>,
    ) -> wasmtime::Result<Vec<(String, Vec<u8>)>> {
        self.guest.call_entries(store, self.resource)
    }
}

struct OutgoingRequest<'a> {
    guest: bindings::exports::wasi::http::types::GuestOutgoingRequest<'a>,
    resource: wasmtime::component::ResourceAny,
}

impl<'a> OutgoingRequest<'a> {
    pub fn new<T>(
        instance: &'a VirtualizedApp,
        store: &mut wasmtime::Store<T>,
        fields: Fields,
    ) -> anyhow::Result<Self> {
        let guest = instance.wasi_http_types().outgoing_request();
        let resource = guest.call_constructor(store, fields.resource)?;
        Ok(Self { guest, resource })
    }

    pub fn set_path_with_query<T>(
        &self,
        store: &mut wasmtime::Store<T>,
        path: Option<&str>,
    ) -> anyhow::Result<Result<(), ()>> {
        self.guest
            .call_set_path_with_query(store, self.resource, path)
    }
}

struct IncomingRequest<'a> {
    #[allow(dead_code)]
    guest: bindings::exports::wasi::http::types::GuestIncomingRequest<'a>,
    resource: wasmtime::component::ResourceAny,
}

impl<'a> IncomingRequest<'a> {
    fn new<T>(
        instance: &'a VirtualizedApp,
        store: &mut wasmtime::Store<T>,
        outgoing_request: OutgoingRequest,
    ) -> anyhow::Result<Self> {
        let guest = instance.fermyon_spin_wasi_virt_http_helper();
        let resource = guest.call_new_request(store, outgoing_request.resource, None)?;
        let guest = instance.wasi_http_types().incoming_request();
        Ok(Self { guest, resource })
    }
}

fn new_response<'a, T>(
    instance: &'a VirtualizedApp,
    store: &mut wasmtime::Store<T>,
) -> anyhow::Result<(wasmtime::component::ResourceAny, ResponseReceiver<'a>)> {
    let guest = instance.fermyon_spin_wasi_virt_http_helper();
    let (out_param, rx) = guest.call_new_response(store)?;
    let rx_guest = instance
        .fermyon_spin_wasi_virt_http_helper()
        .response_receiver();

    Ok((
        out_param,
        ResponseReceiver {
            instance,
            resource: rx,
            guest: rx_guest,
        },
    ))
}

struct ResponseReceiver<'a> {
    instance: &'a VirtualizedApp,
    guest: bindings::exports::fermyon::spin_wasi_virt::http_helper::GuestResponseReceiver<'a>,
    resource: wasmtime::component::ResourceAny,
}

impl<'a> ResponseReceiver<'a> {
    fn get<T>(
        &self,
        store: &mut wasmtime::Store<T>,
    ) -> anyhow::Result<Option<IncomingResponse<'a>>> {
        let Some(resource) = self.guest.call_get(store, self.resource)? else {
            return Ok(None);
        };
        Ok(Some(IncomingResponse {
            instance: self.instance,
            guest: self.instance.wasi_http_types().incoming_response(),
            resource,
        }))
    }
}

struct IncomingResponse<'a> {
    instance: &'a VirtualizedApp,
    guest: bindings::exports::wasi::http::types::GuestIncomingResponse<'a>,
    resource: wasmtime::component::ResourceAny,
}

impl<'a> IncomingResponse<'a> {
    fn status<T>(&self, store: &mut wasmtime::Store<T>) -> anyhow::Result<u16> {
        self.guest.call_status(store, self.resource)
    }

    fn headers<T>(&self, store: &mut wasmtime::Store<T>) -> anyhow::Result<Fields> {
        let fields = self.guest.call_headers(store, self.resource)?;
        Ok(Fields {
            guest: self.instance.wasi_http_types().fields(),
            resource: fields,
        })
    }

    fn consume<T>(
        &self,
        store: &mut wasmtime::Store<T>,
    ) -> wasmtime::Result<Result<IncomingBody<'a>, ()>> {
        let Ok(resource) = self.guest.call_consume(store, self.resource)? else {
            return Ok(Err(()));
        };
        let guest = self.instance.wasi_http_types().incoming_body();
        Ok(Ok(IncomingBody {
            instance: self.instance,
            guest,
            resource,
        }))
    }
}

struct IncomingBody<'a> {
    instance: &'a VirtualizedApp,
    guest: bindings::exports::wasi::http::types::GuestIncomingBody<'a>,
    resource: wasmtime::component::ResourceAny,
}

impl<'a> IncomingBody<'a> {
    fn stream<T>(
        &self,
        store: &mut wasmtime::Store<T>,
    ) -> wasmtime::Result<Result<InputStream, ()>> {
        let Ok(resource) = self.guest.call_stream(store, self.resource)? else {
            return Ok(Err(()));
        };
        Ok(Ok(InputStream {
            instance: self.instance,
            resource,
        }))
    }
}

struct InputStream<'a> {
    instance: &'a VirtualizedApp,
    resource: wasmtime::component::ResourceAny,
}

impl<'a> InputStream<'a> {
    fn blocking_read<T>(
        &self,
        store: &mut wasmtime::Store<T>,
        max_bytes: u64,
    ) -> wasmtime::Result<Result<Vec<u8>, bindings::exports::wasi::io::streams::StreamError>> {
        self.instance
            .wasi_io_streams()
            .input_stream()
            .call_blocking_read(store, self.resource, max_bytes)
    }
}

struct StoreData {
    manifest: String,
    ctx: wasmtime_wasi::WasiCtx,
    table: wasmtime::component::ResourceTable,
}

impl StoreData {
    fn new(manifest: String) -> Self {
        let ctx = wasmtime_wasi::WasiCtxBuilder::new().inherit_stdio().build();
        let table = wasmtime::component::ResourceTable::default();
        Self {
            manifest,
            ctx,
            table,
        }
    }
}

impl wasmtime_wasi::WasiView for StoreData {
    fn table(&mut self) -> &mut wasmtime_wasi::ResourceTable {
        &mut self.table
    }

    fn ctx(&mut self) -> &mut wasmtime_wasi::WasiCtx {
        &mut self.ctx
    }
}

impl bindings::VirtualizedAppImports for StoreData {
    fn get_manifest(&mut self) -> String {
        self.manifest.clone()
    }
}