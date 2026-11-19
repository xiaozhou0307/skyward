# turner recovery compatibility resources

Everything here is what the turner build needs beside the Rust source. Each file
says where it came from, so that a published tree keeps its provenance straight.

## Taken from the working recovery image the user supplied

These are copied verbatim from `skyward_turner_4.0.3_bug已修.img`
(SHA-256 `dd080c5c47c948ab74434168f8fc878542b2e410daa2479162b25c7d14d2fddf`),
which is a recovery image of this model that already worked. They are **not**
taken from the phone's own `/system` or `/vendor` partitions: neither carries a
`libkmsglog.so`, which was checked on the running system.

- `libkmsglog.so`: reference RECOVERY path `skyward/libkmsglog.so`,
  SHA-256 `66e696a51fefac013ee4fe2d2082d5455997876e6190adc12230c78efc9d2a3d`.
  ARM64 shared library using libc/libdl. It forwards Android logging and exports
  `AIBinder_Class_setTransactionCodeToFunctionNameMap`; disassembly shows this
  export logs once and returns, dropping diagnostic transaction names when the
  recovery Binder NDK library lacks the API. It also intercepts
  abort/send/sendto. **Source and licence were not supplied with the image**, so
  this file cannot be described as authored by this project: it is redistributed
  as a device blob and should be replaced by its real source wherever that is
  available.
- `ld.config.txt`: reference `system/etc/ld.config.txt`,
  SHA-256 `a08ae3766847dd03cf3a95cf2e3989e986856dddeef1b209e80a39a238be0260`.
  Non-isolated recovery namespace with recovery system library search path.

The three key-service configuration blocks retain the reference's
recovery-first Binder library ordering, root credentials, and LD_PRELOAD for the
shim. Display/touch configuration is intentionally not copied. On-device KeyMint
registration and actual decryption remain unverified for the new build.

## Written for this port

- `wpaaidl.c`, and `libwpaaidl.so` built from it
  (SHA-256 `53e91e96a982d312d98ffc1583a7002eeacaa6e9472ef9999fead537e4849a64`).
  It is under the same licence as the rest of this project (GPL-3.0).
  It interposes one function, `AServiceManager_addService`, because the
  supplicant of this Android generation ends by itself when the recovery's
  service directory refuses its AIDL interface: the log showed
  `addService(...ISupplicant/default) = -129` followed by exit 255. Nothing in
  the recovery uses that interface — the WLAN is driven over the supplicant's
  control socket — so the registration is answered as done. It is preloaded
  only for the supplicant.
  Build, with the NDK's clang for arm64 (API 30 matches the rest of the build):

  ```sh
  $ANDROID_NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/clang \
      --target=aarch64-linux-android30 \
      --sysroot=$ANDROID_NDK/toolchains/llvm/prebuilt/linux-x86_64/sysroot \
      -shared -fPIC -O2 -o libwpaaidl.so wpaaidl.c
  ```

## Rebuilt from another project

- `kallrecon.ko` (SHA-256
  `03476220edcf58e61cf067d09721e42e75c4d91eb4a7e41645ec4cdbd06a565b`): rebuilt
  from **KallRecon** (GPL-2.0-only, Copyright (C)
  2026 dere3046), which reconstructs the kernel's symbol table and rounds the
  protected-symbol checks the vendor modules trip over. The rebuild carries one
  fix: `unregister_kretprobe()` leaves `KPROBE_FLAG_DISABLED` set and
  `register_kprobe()` keeps it, so the hooks reported being armed while staying
  disabled. The probe flags are now cleared between an unregister and the next
  register. The copy inside the supplied recovery image does not carry that fix
  and stops permitting the checks after the first arming.
  It is a separate kernel module, distributed here as its own file under its own
  licence (GPL-2.0-only, which is not the GPL-3.0 of this project — the two run
  as separate programs through the kernel, they are not linked together).
  Whoever publishes this tree must publish or point at the corresponding
  KallRecon source as well.
