//! platcheck — pin the eo9:platform provider's typed contract in one transcript.
//!
//! Targets the `eo9-examples:platcheck/platcheck` world (see `wit/world.wit`): the
//! live half of the platform provider's refusal test suite. Each probe prints what it
//! asked, what came back, and `ok`/`CONTRACT VIOLATION`; a violation is the program's
//! typed failure, so the scripted battery (`check-usb`) fails loudly on a provider
//! regression.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;

use eo9_guest::api::platform::platform;
use eo9_guest::text;

eo9_guest::bindings!({
    world: "platcheck",
    apis: [platform, text],
});

eo9_guest::main! {
    async fn main(expect_denied: Option<String>) -> Result<ProgramSuccess, ProgramFailure> {
        let io_failure = |err: text::TextError| ProgramFailure::Io(format!("{err:?}"));
        let expect_denied = match expect_denied {
            None => Some(String::from("pl061-gpio")),
            Some(name) if name.is_empty() => None,
            Some(name) => Some(name),
        };
        let mut probes: u32 = 0;

        let root = platform::default();

        // Probe 1: enumerate — the grant's view of the machine table.
        let regions = platform::enumerate(&root).await.map_err(|err| match err {
            platform::PlatformError::Denied => ProgramFailure::Denied,
            other => ProgramFailure::Io(format!("enumerate: {other:?}")),
        })?;
        for region in &regions {
            text::write_out_line(&format!(
                "platcheck: region {} ({} bytes, irq: {})",
                region.name, region.size, region.has_irq,
            ))
            .map_err(io_failure)?;
        }
        if regions.is_empty() {
            return Err(ProgramFailure::NoRegions);
        }
        probes += 1;
        let target = &regions[0];

        // Probe 2: claim the first granted region and read its first register.
        let claimed = platform::claim(&root, target.name.clone()).await.map_err(|err| {
            ProgramFailure::Contract(format!(
                "claim({}) should succeed under this grant, got {err:?}",
                target.name,
            ))
        })?;
        let value = platform::read(&claimed, 0, platform::AccessWidth::Dword)
            .await
            .map_err(|err| {
                ProgramFailure::Contract(format!(
                    "read({}, 0, dword) should succeed, got {err:?}",
                    target.name,
                ))
            })?;
        text::write_out_line(&format!(
            "platcheck: claimed {} and read offset 0: {value:#010x} - ok",
            target.name,
        ))
        .map_err(io_failure)?;
        probes += 1;

        // Probe 3: a second claim of the same name answers busy (machine-wide
        // exclusivity, while the first handle is alive).
        match platform::claim(&root, target.name.clone()).await {
            Err(platform::PlatformError::Busy) => {
                text::write_out_line(&format!(
                    "platcheck: second claim({}) answered busy - ok",
                    target.name,
                ))
                .map_err(io_failure)?;
                probes += 1;
            }
            other => {
                return Err(ProgramFailure::Contract(format!(
                    "second claim({}) must answer busy, got {other:?}",
                    target.name,
                )));
            }
        }

        // Probe 4: a read past the region's end answers out-of-range (typed, never a
        // trap; the width would also overrun, and unaligned offsets refuse the same
        // way).
        match platform::read(&claimed, target.size, platform::AccessWidth::Dword).await {
            Err(platform::PlatformError::OutOfRange) => {
                text::write_out_line(&format!(
                    "platcheck: read({}, {:#x}, dword) answered out-of-range - ok",
                    target.name, target.size,
                ))
                .map_err(io_failure)?;
                probes += 1;
            }
            other => {
                return Err(ProgramFailure::Contract(format!(
                    "read past the end of {} must answer out-of-range, got {other:?}",
                    target.name,
                )));
            }
        }

        // Probe 5: a present-but-ungranted region answers denied — the cross-region
        // containment the per-name grant (`platform=<name>,…`) exists for.
        if let Some(ref ungranted) = expect_denied {
            match platform::claim(&root, ungranted.clone()).await {
                Err(platform::PlatformError::Denied) => {
                    text::write_out_line(&format!(
                        "platcheck: claim({ungranted}) outside the grant answered denied - ok",
                    ))
                    .map_err(io_failure)?;
                    probes += 1;
                }
                other => {
                    return Err(ProgramFailure::Contract(format!(
                        "claim({ungranted}) outside the grant must answer denied, got {other:?}",
                    )));
                }
            }
        }

        // Probe 6: a name no machine table carries answers not-found.
        match platform::claim(&root, String::from("no-such-region")).await {
            Err(platform::PlatformError::NotFound) => {
                text::write_out_line(
                    "platcheck: claim(no-such-region) answered not-found - ok",
                )
                .map_err(io_failure)?;
                probes += 1;
            }
            other => {
                return Err(ProgramFailure::Contract(format!(
                    "claim(no-such-region) must answer not-found, got {other:?}",
                )));
            }
        }

        text::write_out_line(&format!(
            "platcheck: {probes} probe(s) answered exactly as the contract promises",
        ))
        .map_err(io_failure)?;
        Ok(ProgramSuccess::Probes(probes))
    }
}
