//! Placeholder guest component.
//!
//! Exists only to prove the guest build flow (wit-bindgen bindings, cargo build for
//! wasm32-unknown-unknown, `wasm-tools component new`) produces a valid component.
//! Real guest crates (the guest SDK, stubs, eosh, examples) replace this.

wit_bindgen::generate!({
    path: "wit",
    world: "placeholder",
});

struct Component;

impl Guest for Component {
    fn ping() -> u32 {
        9
    }
}

export!(Component);
