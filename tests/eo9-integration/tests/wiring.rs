//! Composition provenance (the wiring tree): every algebra operation records how it built
//! its result, so an interposed provider/attenuator is visible even though it does not
//! appear in the result's residual import/export surface. This is the audit view the
//! security and PL user studies asked for (synthesis #7, study 05 #9): `describe` of
//! `attenuator $ app` looks identical to `describe app`; the wiring tree shows the layer.
//!
//! Provenance is in-memory metadata: it never changes the component bytes, `save()`,
//! `executable_bytes()`, the content hash, or equality (which is byte identity).

use eo9_component::{Component, Wiring, compose, rename, restrict};
use eo9_integration::fixtures;

#[test]
fn compose_wiring_names_the_interposed_provider_that_describe_hides() {
    // text-sink is an attenuator: it satisfies the writer's text import and discards the
    // output. After composition the sink is sealed away from the residual surface.
    let provider = fixtures::text_sink_provider().with_label("text.sink");
    let consumer = fixtures::text_writer().with_label("writer");
    let composed = compose(&provider, &consumer).expect("text-sink $ writer");

    let tree = composed.wiring_tree();
    assert!(tree.contains("$ compose"), "tree:\n{tree}");
    assert!(tree.contains("provider: text.sink"), "tree:\n{tree}");
    assert!(tree.contains("consumer: writer"), "tree:\n{tree}");

    // The interposed attenuator is invisible in the plain describe surface (the exact
    // study finding) but present in the wiring tree above.
    let described = format!("{:?}", composed.describe());
    assert!(
        !described.contains("text.sink") && !described.contains("writer"),
        "describe should not leak the wiring labels: {described}"
    );
}

#[test]
fn restrict_and_rename_record_their_nodes_over_nested_structure() {
    // `only []` over a composition: the inner `answer` import is satisfied (so nothing
    // required is residual and the empty allow-list admits), and the gate is recorded
    // above the compose node.
    let inside = compose(&fixtures::answer_provider(7), &fixtures::answer_consumer())
        .expect("answer-provider $ answer-consumer");
    let gated = restrict(&inside, &[]).expect("only [] $ (provider $ consumer)");
    let tree = gated.wiring_tree();
    assert!(tree.contains("only ["), "tree:\n{tree}");
    assert!(tree.contains("$ compose"), "tree:\n{tree}");

    // rename records a node wrapping the body (the storage consumer's default slot).
    let renamed = rename(
        &fixtures::storage_consumer(),
        "eo9-tests:cap/store",
        "backing",
    )
    .expect("rename the storage slot");
    assert!(
        renamed
            .wiring_tree()
            .contains("rename eo9-tests:cap/store -> backing"),
        "tree:\n{}",
        renamed.wiring_tree()
    );
}

#[test]
fn provenance_never_changes_the_compiled_bytes_or_identity() {
    let composed = compose(&fixtures::text_sink_provider(), &fixtures::text_writer())
        .expect("text-sink $ writer");

    // Reloading from the saved bytes recovers a leaf wiring (the history was never in the
    // bytes) but byte-identical content and an equal component: provenance lives only in
    // memory and never perturbs save()/executable_bytes()/the content hash/equality.
    let reloaded = Component::load(composed.save()).expect("reload composed bytes");
    assert_eq!(composed.save(), reloaded.save());
    assert_eq!(composed.executable_bytes(), reloaded.executable_bytes());
    assert_eq!(
        composed, reloaded,
        "equality is byte identity, not provenance"
    );
    assert!(matches!(composed.wiring(), Wiring::Compose { .. }));
    assert!(matches!(reloaded.wiring(), Wiring::Leaf { .. }));
}
