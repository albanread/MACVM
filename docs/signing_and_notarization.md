# Signing and notarizing macVM — a runbook

Written 2026-08-09 on the Mac Studio, after doing it end to end. Every command
here was run and its output observed; the "expected output" lines are real, not
illustrative. Complements `iCloud Drive → signing/README-mac-studio-setup.md`
(the backup/restore side); this file is the *build and release* side.

Audience: whoever (or whatever) is driving the release — a human, or an
assistant working in this repo.

---

## 0. Rules for an assistant working on this

**Do not create a new certificate.** If `security find-identity` comes back
empty, the fix is to *import the existing backup*, never to mint a fresh one in
Xcode. Reasons, in order of how much they hurt:

- Xcode's "create certificate" flow generates a **new key pair**. It does not
  give you access to the existing identity — it makes a second, unrelated one.
- Developer ID Application certificates are **capped at 5 per team**. Each
  needless one burns a slot permanently.
- Apple never holds your private key. `developer-id-application.p12` is the
  only copy. Everything else is recoverable; that is not.

**Do not type the user's secrets.** Two secrets appear below and both belong to
the human:

| secret | what it unlocks | where it lives |
|---|---|---|
| p12 passphrase (32 chars, machine-generated) | the certificate's private key | password manager |
| app-specific password (`abcd-efgh-ijkl-mnop`) | notarization | password manager |

Hand the human the command; let them run it. Everything else on this page an
assistant can do unattended.

---

## 1. Is the identity present?

```sh
security find-identity -v -p codesigning
```

Expected:

```
  1) CC39AA01407B1ABCE73F791211B36366772E9CE9 "Developer ID Application: ALBAN DOMINIC READ (8T5K8XJSZR)"
     1 valid identities found
```

`0 valid identities found` → go to §2. Anything else present and valid → skip
to §3.

Identity facts, for cross-checking:

- **Team ID** 8T5K8XJSZR, Apple ID albanread@googlemail.com
- **SHA-1** `CC:39:AA:01:40:7B:1A:BC:E7:3F:79:12:11:B3:63:66:77:2E:9C:E9`
- **Valid** 2026-08-07 → 2031-08-08

---

## 2. Restoring the identity (only when §1 says 0)

The backup lives in `iCloud Drive → signing/certificates/`. Make sure the files
are materialized first — iCloud keeps them as placeholders, and `ls -l` shows a
plausible size even when the bytes are not local. `du` reporting `total 0` is
the tell.

```sh
brctl download ~/Library/Mobile\ Documents/com~apple~CloudDocs/signing/certificates/*
```

Install Apple's intermediate first (public certificate, no secret, safe for an
assistant to run):

```sh
security import ~/Library/Mobile\ Documents/com~apple~CloudDocs/signing/certificates/DeveloperIDG2CA.cer \
  -k ~/Library/Keychains/login.keychain-db
```

Then the p12 — **the human runs this one**:

```sh
security import ~/Library/Mobile\ Documents/com~apple~CloudDocs/signing/certificates/developer-id-application.p12 \
  -k ~/Library/Keychains/login.keychain-db \
  -P "$(tr -d '\r\n' < ~/Desktop/signing-p12-passphrase.txt)" \
  -T /usr/bin/codesign -T /usr/bin/security
```

Expected: `1 identity imported.` (plus `1 certificate imported.`)

Notes that cost real time on 2026-08-09:

- **The trailing newline is the trap.** The passphrase file is 33 bytes: 32
  characters + `\n`. Pasting it into the Keychain Access dialog by selecting the
  line takes the newline with it and the dialog says the password is wrong. The
  `tr -d '\r\n'` above is the whole fix. Double-clicking the p12 in Finder works
  *only* if you paste exactly 32 characters.
- To check a candidate passphrase without importing anything, and without
  printing it:
  ```sh
  openssl pkcs12 -in developer-id-application.p12 -noout -passin pass:CANDIDATE && echo OK
  ```
- `-T /usr/bin/codesign` pre-authorises codesign against the key, so signing
  does not raise a keychain prompt on every run.
- If the identity imports but §1 still reports 0 valid, the Apple intermediate
  is missing — install `DeveloperIDG2CA.cer` as above.

Verify the chain:

```sh
security verify-cert -c ~/Library/Mobile\ Documents/com~apple~CloudDocs/signing/certificates/developerID_application.cer -p codeSign
```

Expected: `...certificate verification successful.`

---

## 3. Building and signing

**Use the script. Do not hand-roll `codesign`.**

```sh
SIGN_ID="Developer ID Application: ALBAN DOMINIC READ (8T5K8XJSZR)" tools/make-macapp.sh cocoa
```

Output lands in `dist/` — `macVM.app` and `macVM.dmg`. The script signs nested
code before the bundle, applies `--options runtime` (hardened runtime, required
for notarization) and `--timestamp` (a secure timestamp, also required —
so **the machine must be online**), and attaches `tools/macvm.entitlements`.

Why the entitlements are not optional: MACVM is a JIT. It maps its code cache
`MAP_JIT` and flips W^X with `pthread_jit_write_protect_np`. The hardened
runtime refuses `MAP_JIT` without `com.apple.security.cs.allow-jit`. Sign
without it and the app installs, notarizes, launches — and then dies the
instant a method tiers up. `JitMode::Off` is not a supported configuration for
the embedded VM, so there is no fallback.

Confirm the signature:

```sh
codesign -dv --verbose=4 dist/macVM.app 2>&1 | grep -E "Authority|Timestamp|Runtime|TeamIdentifier"
codesign --verify --deep --strict --verbose=2 dist/macVM.app
codesign -d --entitlements - dist/macVM.app | grep -i jit
```

Expected, all four lines present:

```
Authority=Developer ID Application: ALBAN DOMINIC READ (8T5K8XJSZR)
Authority=Developer ID Certification Authority
Authority=Apple Root CA
Timestamp=9 Aug 2026 at 12:08:08
TeamIdentifier=8T5K8XJSZR
Runtime Version=26.2.0
dist/macVM.app: valid on disk
dist/macVM.app: satisfies its Designated Requirement
	<key>com.apple.security.cs.allow-jit</key>
```

At this point Gatekeeper still refuses it, correctly:

```sh
spctl -a -vv dist/macVM.app
# macVM.app: rejected
# source=Unnotarized Developer ID
```

Signed but not notarized. That is §4.

---

## 4. Notarizing

### 4a. FIRST: does a profile already exist?

**Check before creating.** `notarytool` has no "list profiles" command, which is
exactly how an existing credential hides — and hunting for the wrong *name* looks
identical to having no credential at all. On 2026-08-09 this cost an afternoon
and a locked Apple ID: the notes said `AC_NOTARY`, the machine actually had a
working profile called **`macvm`**, and every retry of `store-credentials`
re-authenticated against Apple until the account tripped its failed-auth
threshold.

Probe the likely names — each takes a second, and a hit means you are done:

```sh
for p in macvm AC_NOTARY notary; do
  echo "--- $p"; xcrun notarytool history --keychain-profile "$p" 2>&1 | head -2
done
```

`Successfully received submission history.` = that profile works; use that name
in §4b and skip the rest of this section. Also grep your shell history, which is
where the real answer was hiding:

```sh
grep -a "store-credentials" ~/.zsh_history | sed 's/^: [0-9]*:[0-9]*;//'
```

### 4b. Only if no profile exists — the human runs this

There is no notary key in the backup; credentials are per-machine.

```sh
xcrun notarytool store-credentials "AC_NOTARY" --apple-id albanread@googlemail.com --team-id 8T5K8XJSZR
```

Omit `--password` deliberately: notarytool then prompts, so the app-specific
password stays out of shell history. Generate one at
account.apple.com → Sign-In & Security → App-Specific Passwords if needed; the
label you give it there (e.g. "notaryapp") is only for your own bookkeeping and
is not part of the credential.

Confirm it stored — this is the check that matters, because a failed
`store-credentials` is silent until you try to use it:

```sh
xcrun notarytool history --keychain-profile "AC_NOTARY"
```

`Error: No Keychain password item found for profile: AC_NOTARY` means it did
**not** store. Nothing in §4b will work until this command lists history
(an empty history is fine — the point is that it authenticates).

### 4c. Per release — one command

`make-macapp.sh` does the whole chain when both variables are set:

```sh
SIGN_ID="Developer ID Application: ALBAN DOMINIC READ (8T5K8XJSZR)" NOTARY_PROFILE=macvm \
  tools/make-macapp.sh cocoa
```

It signs, notarizes the **.app** and staples it, packages the dmg from the
stapled bundle, signs the dmg, notarizes and staples that too. Two submissions,
a few minutes each.

**Why the app is stapled before packaging:** a ticket on the dmg does not
travel with the app when a user drags it to /Applications, so that copy can
only be validated by calling Apple — which stalls or fails on a first launch
that is offline. Verified 2026-08-09: before the fix the shipped dmg passed
`spctl` while the app inside answered *"does not have a ticket stapled"*.
The check that matters:

```sh
hdiutil attach dist/macVM.dmg -nobrowse
ditto /Volumes/macVM/macVM.app /tmp/dragtest/macVM.app   # simulate the drag
xcrun stapler validate /tmp/dragtest/macVM.app           # must say: worked
```

### 4c-manual. The same steps by hand

Notarize the DMG (it contains the app; one submission covers both):

```sh
xcrun notarytool submit dist/macVM.dmg --keychain-profile "macvm" --wait
```

Use the profile name §4a found — on this machine it is **`macvm`**. Run it from
the repo root, or give an absolute path: `notarytool` reports a missing file as
`The file couldn't be opened because it doesn't exist`, which reads like a
notarization problem and is really a `cd` problem.

`--wait` blocks until Apple answers, typically a few minutes. Expect
`status: Accepted`. On `Invalid`, get the reason — it is always specific:

```sh
xcrun notarytool log <submission-id> --keychain-profile "macvm"
```

Then staple, so the ticket travels with the file and Gatekeeper is satisfied
offline:

```sh
xcrun stapler staple dist/macVM.dmg
xcrun stapler staple dist/macVM.app
```

Staple bundles, dmgs and pkgs — **not** bare binaries; there is nowhere to put
the ticket and it will fail.

Final check, the one that proves a user can open it:

```sh
spctl -a -vv dist/macVM.app
```

Expected:

```
dist/macVM.app: accepted
source=Notarized Developer ID
```

---

### 4d. Verified end to end, 2026-08-09

Submission `8d2db906-4170-4c17-a6dd-0bbc6793aa45` → `status: Accepted`, both
artifacts stapled, and:

```
dist/macVM.app: accepted
source=Notarized Developer ID
origin=Developer ID Application: ALBAN DOMINIC READ (8T5K8XJSZR)
```

`xcrun stapler validate` passes, so the ticket travels with the file and
Gatekeeper is satisfied with no network.

### 4e. If Apple returns 401 / "your Apple ID has been locked"

Rate limiting from repeated failed authentications, not a security incident.
`store-credentials` validates against Apple on every attempt, so a loop of
retries — especially with the wrong profile name, or an Apple ID password where
an app-specific one belongs — will trip it.

- Try **unlock** before **reset**: a trusted device (Settings → your name →
  Sign-In & Security), or signing in at appleid.apple.com. An unlock keeps your
  password and your existing app-specific passwords.
- A **reset** revokes every app-specific password you have ever issued, so
  generate a new one afterwards or you will keep seeing 401 and think the lock
  never lifted.
- Avoid reset with no trusted device to hand: it can start account recovery,
  which takes days and locks you out of iCloud — where this signing backup
  lives.
- None of this touches signing. The certificate, the private key and any
  already-signed artifacts are local and unaffected; only the notary service
  call is blocked.

## 5. Renewal and loss

- Certificate expires **2031-08-08**. Renew at developer.apple.com →
  Certificates, uploading the **same CSR** from
  `signing/certificates/DeveloperID.certSigningRequest` — that keeps the same
  key, so already-distributed apps stay consistent.
- If the p12 ever leaks: revoke at developer.apple.com. Apps already notarized
  and stapled keep working.
- Rebuilding a p12 on a Mac: Homebrew OpenSSL 3's default `pkcs12 -export`
  output is **rejected by the macOS keychain** ("MAC verification failed"). Pin
  legacy parameters and always test-import into a scratch keychain first:
  ```sh
  openssl pkcs12 -export -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 \
    -inkey DeveloperID.key -in cert.pem -certfile g2ca.pem -name "$IDENT" -out out.p12
  ```

## 6. Hygiene

`~/Desktop/signing-p12-passphrase.txt` should not survive setup. The Desktop
syncs to iCloud, and the encrypted p12 lives in that same iCloud account —
passphrase and ciphertext together defeat the point of encrypting the backup.
Move the passphrase to the password manager and delete the file.
