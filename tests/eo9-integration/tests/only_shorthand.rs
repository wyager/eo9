//! `only` package-shorthand (plan/10): an allow-list entry that names just a package
//! (`eo9-tests:cap`) admits every interface of that package the consumer imports, in
//! addition to the existing full-interface form (`eo9-tests:cap/answer`). A genuinely
//! missing capability is still refused, so the shorthand is purely a convenience.

use eo9_component::{InterfaceRef, RestrictError, restrict};
use eo9_integration::fixtures;

/// A package shorthand admits an interface of that package that the full form admits.
#[test]
fn package_shorthand_admits_an_interface_of_that_package() {
    let consumer = fixtures::answer_consumer(); // requires eo9-tests:cap/answer
    // Full form: accepted (no offenders, nothing to seal).
    restrict(&consumer, &[InterfaceRef::any("eo9-tests:cap/answer")])
        .expect("full interface ref admits the import");
    // Package shorthand: the same import is admitted by naming just the package.
    restrict(&consumer, &[InterfaceRef::any("eo9-tests:cap")])
        .expect("package shorthand admits any interface of that package");
}

/// The shorthand does not over-admit: a *different* package still refuses the import.
#[test]
fn a_different_package_shorthand_still_refuses() {
    let consumer = fixtures::answer_consumer();
    let err = restrict(&consumer, &[InterfaceRef::any("eo9-tests:other")]).unwrap_err();
    assert!(
        matches!(err, RestrictError::RequiredOutsideAllowList(ref o)
            if o.iter().any(|e| e.contains("eo9-tests:cap/answer"))),
        "an unrelated package shorthand must still refuse the import: {err:?}"
    );
}

/// Full and shorthand entries may be mixed in one allow-list.
#[test]
fn mixed_full_and_shorthand_entries_work() {
    let consumer = fixtures::answer_consumer();
    restrict(
        &consumer,
        &[
            InterfaceRef::any("eo9-tests:cap"),
            InterfaceRef::any("eo9-tests:other/thing"),
        ],
    )
    .expect("a package shorthand alongside an unrelated full ref still admits the import");
}
