//! The integration suites that exercise real provider chains (the converted async-first
//! stubs: storage, net, switch ports, interposed filters) plus the algebra/soundness
//! suites — verbatim from tests/eo9-integration/tests, run against the vendored wasmtime
//! in both arms of the first-poll A/B. Same outcomes expected in both arms.
//!
//! These need the guest components built first: `cargo xtask build-guest` from the
//! repository root (eo9_integration::guest locates the artifacts by manifest path).

#[path = "../../eo9-integration/tests/eofs.rs"]
mod eofs;

#[path = "../../eo9-integration/tests/pci_filtered.rs"]
mod pci_filtered;

#[path = "../../eo9-integration/tests/net_l4_over_l2.rs"]
mod net_l4_over_l2;

#[path = "../../eo9-integration/tests/vnic_switch.rs"]
mod vnic_switch;

#[path = "../../eo9-integration/tests/interposition.rs"]
mod interposition;

#[path = "../../eo9-integration/tests/compound_config.rs"]
mod compound_config;

#[path = "../../eo9-integration/tests/algebra_properties.rs"]
mod algebra_properties;

#[path = "../../eo9-integration/tests/soundness_corpus.rs"]
mod soundness_corpus;
