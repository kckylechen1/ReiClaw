# async-imap downstream time migration

## Source and ownership

This directory contains the complete protocol implementation and examples from
[chatmail/async-imap](https://github.com/chatmail/async-imap), commit
[`d1f2606421c9211f992c1c565a32d8f770476aeb`](https://github.com/chatmail/async-imap/commit/d1f2606421c9211f992c1c565a32d8f770476aeb),
the revision previously locked by ZeroClaw. The package retains its `async-imap`
name, version 0.12.0, original authors, and MIT OR Apache-2.0 licensing; both
license files are included. ZeroClaw's maintainers own the downstream changes.

The original README and CHANGELOG are retained as upstream documentation.
This file records the additional downstream work. No newer unrelated upstream
changes are included.

## Downstream changes

- Replace the Chrono dependency with Jiff 0.2.46.
- Return `jiff::Zoned` from `Fetch::internal_date()` and parse the same IMAP
  date format while preserving the original fixed UTC offset.
- Preserve `None` for absent dates, invalid calendar values, and invalid offsets.
  Offsets remain bounded to less than 24 hours. Leap seconds also return `None`,
  because Jiff otherwise silently normalizes second 60 to second 59.
  Retain Chrono's minute-precision offset syntax (colon/whitespace separators
  and either minus sign), reject offset seconds, and detect leap seconds even
  when the seconds field starts with whitespace.
- Keep the network protocol, authentication, commands, buffering, and pin
  projection implementation unchanged.

ZeroClaw does not use `internal_date()` in its email read/search production path.
The existing wire-level email tests continue to exercise this retained client;
an additional FETCH regression exercises the changed accessor through the
public protocol API. This is a source-level replacement of the date dependency,
not an advisory exception or a package rename.

## Maintenance and exit condition

Continue reviewing upstream protocol and security changes against the pinned
revision. Keep this directory outside the first-party Cargo workspace, but
include the path dependency in dependency, license, and SBOM checks.

Remove the downstream copy when an upstream version removes Chrono and passes
the ZeroClaw LOGIN/XOAUTH2, SEARCH/FETCH rejection, literal fragmentation, EOF,
BYE, IDLE, and date-offset regressions. Update the dependency and lockfile
together, verify that Chrono remains absent from every supported feature graph,
then remove this directory. Do not relax advisory or duplicate-version policy
to complete that transition.
