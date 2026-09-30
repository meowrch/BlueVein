# Windows LE identity synchronization repair

## Pairing removal protocol (candidate, 2026-09-20)

A local bond absent at startup is not treated as deleted. The live monitor must
first have observed it after a successful synchronization, then observe its
removal. Windows waits for three seconds of continuous absence; Linux confirms
the device directory remains absent after two seconds. The EFI record keeps a
pending-deletion marker with the source OS and SHA-256 fingerprints of the
observed bonding keys; the fingerprints are never logged. The source OS does
not reimport that record.

On the other OS, the sync removes the local pairing through the OS Bluetooth
API only when its bonding keys match the marker. Linux asks BlueZ `RemoveDevice`
over D-Bus; Windows calls `BluetoothRemoveDevice`. After confirming the local
record is gone, BlueVein removes the EFI record. If the local record was already
absent, it only clears EFI. A different complete bond at the same address wins
over the old marker; an uncomparable record or failed unpair leaves the marker
for diagnosis. `audit-sync` previews the removal without changing either OS.

Both OS binaries must be updated before relying on this protocol. An older
binary accepts the unknown marker fields while reading JSON, but its next EFI
write serializes the older schema and erases `name` and `pending_deletion`.
The automated tests cover the state machine, but physical unpair behavior on
the dual-boot host is not yet verified; do not use a working InpuDeck/MX bond
as the first live deletion test.

### Removal failures observed on the dual-boot host (2026-09-21)

`BluetoothRemoveDevice` returned `ERROR_NOT_FOUND` (1168) for a marked LE peer:
the Windows stack had no device object, while BTHPORT still held its keys, which
is exactly the record BlueVein reads and would keep re-exporting. That code is
now treated as "already unpaired", and only the leftover BTHPORT key material
for that identity is deleted: the Classic link-key value and every LE subkey
whose `Address` resolves to the same peer. Other unpair errors still fail.

A failed removal no longer aborts the run. The marker stays in EFI, the rest of
the merge is published, and the failures are reported once as `UnfinishedRemovals`
after the EFI write. Service startup continues into monitoring for that error
only; any other startup failure still stops before the export monitor, so an
incomplete import can never be mistaken for a clean state.

## Device names

Names are synchronized as ordinary shared metadata and never gate key handling.
Windows reads the BTHPORT `Devices` name cache for both the identity and the
LE storage record. That cache is empty for peers that never reported a name
while paired, so an old LE bond can have keys and no cached name; BlueVein then
reads `FriendlyName` from the PnP device container under `Enum\BTHLE` or
`Enum\BTHENUM`. Per-service nodes are ignored: they name a profile, not a peer.
Linux reads the BlueZ `Name` and falls back to a user `Alias`.

A name discovered after the bond was exported is written into the existing EFI
record, including when `update_reason` refuses to import that device's keys.
Windows keeps names out of its registry projection, so a name change never
rewrites registry keys, and unchanged names never rewrite EFI. Log lines carry
`ADDRESS (Name)` so a journal identifies devices; key material is never logged.

## General Linux bond import

Linux now imports complete missing bonds for locally known adapters, including
an adapter with no paired devices. There is no device-name or MAC allowlist.
Windows still requires an existing OS bond: creating registry keys alone is not
PnP device enrollment. Pair a new device once in Windows, then export it for Linux.

New shared IRKs carry `irk_encoding: "windows"`. Both backends export this format;
Linux reverses bytes at its BlueZ storage boundary, while Windows uses registry
bytes directly. LTK bytes are unchanged. `random` in the shared record maps to
BlueZ's on-disk `static`; public identities remain public.

Unmarked legacy IRKs are tagged only when they match the current backend's
canonical key. A different unmarked IRK is ambiguous and is reported without
importing it. Run the updated Windows exporter to establish the format of old
Windows records before Linux import. The former per-peer
`BLUEVEIN_LINUX_REVERSE_IRK_PEERS` override is no longer read. Do not globally
reverse old shared keys based on their device names or assume their origin.

Missing LE bonds require a stable public/static identity, a complete LTK (key,
authentication/type, encryption size, EDIV, Rand), and a known format for any IRK.
Incomplete records are preserved in EFI and reported individually; they do not
prevent other complete bonds from importing. IRK-only discovery records and
transient private addresses are not manufactured into bonds. A record containing
only address metadata is not exported as a bond.

Linux stops Bluetooth once before writing an import batch, then starts it once
afterwards, including on an import error. This prevents a running bluetoothd from
flushing stale records over imported keys. Unchanged synchronization does not
stop Bluetooth. Complete info files are published atomically with mode 0600;
unrelated fields in existing records are retained. New imported bonds are trusted
and declare their supported transports. No scanning or reconnect loop is added.

Deletion requests are processed before an import batch because Linux removes a
bond through the running BlueZ D-Bus service. Only after all eligible removals
finish does BlueVein stop bluetoothd, publish key files, and start it once. Both
services poll EFI every 30 seconds. The Linux monitor exports any pending local
snapshot changes before each periodic import and recognizes BlueVein's atomic
`info` rename, so an imported write is not reclassified as a new local pairing.

`sudo bluevein --audit-sync` previews imports and migrations without writing keys
or stopping Bluetooth. `sudo bluevein --sync-once` applies one batch. Normal service
startup uses the same planner. Set `BLUEVEIN_EFI_DEVICE` as for the service.

The automated suite covers several missing devices, empty adapters, incomplete
records, foreign adapters, dry-run/EFI-only behavior, repeated synchronization,
and real BlueZ file round trips with legacy and Secure Connections metadata.
The automated suite does not establish physical reconnect behavior. A Windows
deployment and one subsequent Linux reboot have now been tested on the same
adapter; see the status below.

### Existing host privacy finding

The same host required `Privacy=off` in BlueZ for its public controller identity
to be recognized by the existing iPhone bond. With a reversed Linux IRK, original
LTK, and public host address, the physical capture showed successful AES-CCM
encryption and Linux created keyboard/mouse HID devices. Reversing LTK did not
help. This adapter-wide privacy choice exposes the public Bluetooth address;
Repeated reboot and return-to-Windows acceptance remain required. Other devices
and Macs must retain their pairing/channel settings.

## Root cause verified on a dual-boot host

Windows stored the working HID bond under a pairing-time resolvable private
address. Its registry `Address` value identified the public peer identity used by
Linux. BlueVein 1.2.0 indexed the shared record by the registry subkey name and
ignored `Address` and `AddressType`. Correct Windows LTK/IRK values reached EFI,
but under the temporary address, while the Linux identity record retained an old
LTK and a different IRK.

A separate public-address Windows entry contained only the stale IRK. The fix
prefers the complete identity-mapped bond, refuses conflicting complete bonds,
and routes imports back to the real Windows storage entry.

## Repair and invariants

- Export/import uses the peer identity and its public/static-random address type.
- Legacy EFI aliases migrate only when their LTK, IRK, EDIV, Rand and key length
  match the live Windows bond. Mismatches stop automatic migration.
- Classic keys and other devices are retained. A stale peripheral LTK is replaced
  by the current SC key only for a verified Secure Connections bond; unresolved
  legacy role-specific conflicts stop migration.
- Windows AuthReq SC metadata and zero EDIV/Rand map to the BlueZ MGMT P-256 key
  type. Requested MITM alone is not treated as proof of authentication. Windows
  writes use its boolean Authenticated representation and preserve other flags.
- Missing metadata in older exports does not downgrade the same known LTK.
- Complete Classic/LE snapshots on both operating systems serialize local exports
  before periodic imports.
- Platform-only metadata does not cause endless import loops; unchanged state
  does not rewrite EFI. Read/import errors stop synchronization.
- Windows service status interrogation no longer requests shutdown.

The Linux key type/role behavior is documented by BlueZ MGMT and Linux
`hci_find_ltk`: Secure Connections keys apply independently of central/peripheral
role. Legacy role-specific keys are not interchangeable.

## Commands

`bluevein.exe audit-sync [adapter-mac identity-mac]` runs the same planning code
without modifying the registry or EFI keys. Its diagnostics do not print secrets.

`bluevein.exe repair-efi-only [adapter-mac identity-mac]` writes the shared config
but refuses any plan that would update local Windows keys. Scope the first repair
to the affected peer; take a private backup and stop the old sync service first.

`bluevein.exe sync-once [adapter-mac identity-mac]` performs one synchronization.
Normal service operation remains automatic for all devices.

## Validation and limits

CI exercises real registry round trips in isolated HKCU fixtures, identity/RPA
migration with a stale IRK-only shadow, conflict rejection, SC metadata, LE rekey
handling, unchanged-state behavior, failure handling, and service control. It
builds both Windows and Linux.

A live read-only audit verified that the affected peer's migration requires no
Windows key writes. Deployment additionally checks key equality, unchanged other
EFI records, unchanged Windows registry values, and the actual service process.

On the tested dual-boot host, the updated Windows exporter preserved the live
Windows bonds; the maintainer confirmed InpuDeck, MX Keys and MX Master input
after a Linux-to-Windows transition. On the following Linux boot, BlueVein
reported all three existing bonds correct and skipped the EFI write. All three
attached over LE without a manual Connect, and the InpuDeck user watcher found
HID already ready when it started. This is one successful boot sample; repeated
Windows/Linux transitions and the delayed Windows InpuDeck reconnect remain
unresolved acceptance checks.

The intermittent Linux LE connection timeout was separately traced to
[BlueZ issue #2356](https://github.com/bluez/bluez/issues/2356): a dual-mode
bonded peer advertising with a private address lacked the controller's address
resolution flag. Setting that flag allowed the next HID connection, and a
startup workaround lives in InpuDeck's Linux host setup, not in BlueVein.
BlueVein manages keys, not controller connection flags or reconnect requests.

Startup retains upstream EFI precedence for unrelated conflicting offline edits;
there is no persisted three-way conflict history. This branch must not be described
as resolving arbitrary simultaneous offline pairing changes.

## Deployed Windows validation (2026-09-20)

Commit `4b28aa3` passed 40 Windows tests and 27 Linux tests, with release builds
for both platforms. Its Windows binary was installed as the existing service
using a protected ProgramData directory.

The scoped repair verified current registry LTK/IRK equality at the canonical EFI
identity, removal of the legacy alias, preservation of all other EFI records at
the repair checkpoint, and byte-for-byte preservation of Windows registry values
before and after starting the replacement service. Private pre-change backups
are DPAPI-protected with restricted ACLs. Bluetooth radio/service was not restarted.

The replacement service remained running across multiple periodic checks without
new key-mismatch or error messages. Physical Linux reconnect and return-to-Windows
validation remain outstanding; these results are not a claim of completed hardware
validation.

## Test candidate after the initial deployment

EFI writes now require a complete configuration read-back match. Missing or
unreadable output is an error, so the Windows monitor retries the export before
allowing another import. Linux startup also propagates a failed initial sync
instead of entering its export monitor after an incomplete import.

Regression coverage includes dropped writes, unreadable read-back, retry after a
failed export, both SC security types during rekey, and conflicting complete
Windows identity records. CI runs on pull requests as well as the fork branch.

### Hardware acceptance checklist

1. Keep the existing pairing and boot Linux. Confirm EFI and BlueZ identity,
   LTK/IRK and role-key equality privately; never paste key files into an issue.
2. Verify cursor movement and keyboard input, then disconnect/reconnect the app.
3. Suspend/resume Linux and repeat input and reconnect checks.
4. Boot Windows again and repeat input/reconnect checks. Confirm the service
   remains running and does not repeatedly rewrite unchanged keys.
5. Confirm headphones and the pre-existing Mac pairing still work.

Record OS/BlueZ versions, tested binary commit, pass/fail for each step, and
sanitized error/event summaries. Do not claim hardware validation from CI.

## Follow-up candidate validation (2026-09-20)

Commit `3785471` passed 45 Windows tests and 30 Linux tests, with both release
builds. It was deployed over the initial fix using the same backup, audit and
registry-preservation checks. The maintainer confirmed cursor and keyboard input
still work in Windows. Linux hardware and return-to-Windows checks remain pending.

## Windows re-pair recovery (2026-09-29)

A re-pair can leave two full BTHPORT LE records for one identity: a stale
canonical record imported from EFI, and a working RPA-backed Windows device.
The old monitor rejected every snapshot and logged the same error each second.

The resolver now uses the Configuration Manager present BTHLE device list when
full records conflict. Exactly one candidate must correspond to a present LE
node; zero or multiple candidates still fail closed. This is device enumeration,
not a radio scan, connection attempt or evidence of a currently encrypted link.

Before startup import, EFI can be recovered from that local bond only when its
Windows-encoded LTK and IRK match a rejected shadow record. Both represented
local transports are retained, because re-pairing may renew Classic as well as
LE. Secure Connections role keys are updated together; unresolved legacy role
conflicts are refused. No registry record is deleted as part of this recovery.
Unrelated EFI keys and pending deletion markers are not treated as stale bonds.

Unchanged snapshot errors are logged at most once per minute, with a recovery
message when snapshots succeed again. Tests cover unique/ambiguous enrollment,
real isolated registry duplicates, dual-mode rekey, read-back convergence,
idempotence, unrelated EFI conflicts and preservation of local key material.

API reference: https://learn.microsoft.com/en-us/windows/win32/api/cfgmgr32/nf-cfgmgr32-cm_get_device_id_lista

Live Windows deployment of `3094977` passed read-only audit, EFI-only recovery,
LTK/IRK/SC peripheral equality, full Windows key-registry preservation before and
after service startup, and preservation of other EFI bonds. The first candidate
was refused before writes because Classic had also changed; the final regression
covers both transports. CI passed 65 Windows and 50 Linux tests with both builds.
Cross-OS physical reconnect still needs testing when the host next boots Linux.
