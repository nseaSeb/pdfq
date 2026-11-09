//! Drift guard: `docs/openapi.yaml` lists every error kind and every route.

use std::path::Path;

use pdfq_protocol::ErrorKind;

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn openapi_lists_error_kinds_and_routes() {
    let spec = read("../../docs/openapi.yaml");
    let lines: Vec<&str> = spec.lines().map(str::trim).collect();

    for kind in ErrorKind::ALL {
        let item = format!("- {}", kind.as_str());
        assert!(
            lines.contains(&item.as_str()),
            "error kind `{}` missing from the ErrorKind enum of docs/openapi.yaml",
            kind.as_str()
        );
    }

    let api = read("src/api.rs");
    let routes: Vec<&str> = api
        .split(".route(\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .collect();
    assert!(
        routes.len() >= 7,
        "no routes found in src/api.rs: {routes:?}"
    );
    for route in routes {
        let key = format!("{route}:");
        assert!(
            lines.contains(&key.as_str()),
            "route `{route}` missing from the paths of docs/openapi.yaml"
        );
    }
}
