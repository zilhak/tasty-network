# tasty-net-ssh-config

OpenSSH client configuration (`ssh_config`) parsing for the embedded SSH backend of the Tasty
network, ported from OpenSSH portable. The design is in
[SSH 백엔드](../../docs/design/ssh-backends.md#openssh-해석-이식-크레이트).

## Base version

| Item | Value |
|---|---|
| Release | OpenSSH portable 10.6p2 (`SSH_RELEASE` `OpenSSH_10.6p2`) |
| Tag | `V_10_6_P2` (tag object `e9c0339ed598bd96aa814be2c776b6c3c2357ab4`) |
| Commit | `d449e662b6b8b8cb20e852d8084c269af1c5e8f1` |
| Repository | <https://github.com/openssh/openssh-portable> |

The build the port follows has GSSAPI, PKCS#11, security keys (`ENABLE_SK`) and the
post-quantum algorithms (`USE_MLDSA`, `USE_SNTRUP761X25519`, `USE_MLKEM768X25519`) enabled.
Line numbers below are of that commit; a range includes the return type line before the
function name.

## License

This crate is `BSD-2-Clause AND BSD-3-Clause AND ISC AND SSH-short`. The files derived from
OpenSSH are marked `derived from OpenSSH, modified` and keep the notices of their original
files. [LICENSE-OpenSSH](LICENSE-OpenSSH) is the OpenSSH `LICENCE` file of the base commit.
`src/scaled.rs`, `src/glob.rs` and the `vis_white` function of `src/options.rs` follow
`scan_scaled`, `glob` and `vis` of `openbsd-compat` step by step, so they keep the notices of
those files (BSD-3-Clause; `charclass.h` is in the public domain).

## Ported code

| Rust file | Original file | Functions and tables (lines) |
|---|---|---|
| `src/keywords.rs` | `readconf.c` | `keywords[]` (`:175`–`:325`), `parse_token` (`:958`–`:973`) |
| `src/readconf.rs` | `readconf.c` | `ssh_connection_hash` (`:344`–`:361`), `expand_match_exec_or_include_path` (`:643`–`:694`), `match_cfg_line` (`:699`–`:923`), `process_config_line` and `process_config_line_depth` (`:1132`–`:2611`), `read_config_file` and `read_config_file_depth` (`:2618`–`:2683`, `READCONF_MAX_DEPTH` `:2629`), `parse_fwd_field` (`:3232`–`:3286`), `parse_forward` (`:3298`–`:3437`), `ssh_valid_hostname` (`:3439`–`:3452`), `ssh_valid_ruser` (`:3454`–`:3471`), `parse_jump` (`:3473`–`:3550`), `parse_ssh_uri` (`:3552`–`:3577`) |
| | `readconf.h` | `SSHCONF_*` flags, `MAX_*` limits |
| | `addrmatch.c` | the checks of `addr_match_cidr_list` with no address (`:105`–`:166`: empty entry, `INET6_ADDRSTRLEN + 3` length, `VALID_CIDR_CHARS`) |
| | `addr.c` | `addr_unicast_masklen` (`:37`–`:47`), `masklen_valid` (`:49`–`:60`), `addr_host_is_all0s` (`:333`–`:344`), `addr_pton_cidr` (`:459`–`:495`) |
| `src/options.rs` | `readconf.h` | `struct Options` |
| | `readconf.c` | `add_local_forward` (`:368`–`:390`), `add_remote_forward` (`:397`–`:421`), `add_certificate_file` (`:453`–`:475`), `add_identity_file` (`:477`–`:506`), `multistate_*` tables (`:994`–`:1108`), `parse_multistate_value` (`:1111`–`:1126`), `initialize_options` (`:2713`–`:2838`), `fill_default_options` (`:2859`–`:3135`), `option_clear_or_none` (`:2686`–`:2690`), `fmt_multistate_int` to `dump_cfg_forwards` (`:3580`–`:3736`), `dump_client_config` (`:3738`–`:3968`) |
| | `pathnames.h` | `_PATH_SSH_USER_DIR`, `_PATH_SSH_CLIENT_ID_*`, `_PATH_SSH_SYSTEM_HOSTFILE*`, `_PATH_SSH_USER_HOSTFILE*` |
| | `log.c` | `log_facilities[]` (`:76`), `log_levels[]` (`:97`), `log_facility_number`, `log_facility_name`, `log_level_number`, `log_level_name` (`:117`–`:161`) |
| | `digest-openssl.c` | `digests[]` names (`:58`), `ssh_digest_alg_by_name` (`:79`–`:89`) |
| | `openbsd-compat/vis.c` | `vis` for `VIS_WHITE` (`:61`–`:144`, `isvisible` `:46`) |
| `src/scaled.rs` | `openbsd-compat/fmt_scaled.c` | `scan_scaled` (`:75`–`:229`) |
| `src/glob.rs` | `openbsd-compat/glob.c` | `glob` (`:179`–`:230`), `g_charclass` (`:438`–`:463`), `glob0` (`:472`–`:594`), the joining and length cut of `globtilde` (`:364`–`:410`; the home itself comes from the caller), `glob1` (`:611`–`:622`), `glob2` (`:629`–`:695`), `glob3` (`:697`–`:780`), `match` (`:903`–`:969`) |
| | `openbsd-compat/charclass.h` | `cclasses[]` |
| `src/resolve.rs` | `ssh.c` | `tilde_expand_paths` (`:188`–`:199`), `default_client_percent_expand` and `default_client_percent_dollar_expand` (`:206`–`:232`), `is_addr_fast` and `is_addr` (`:284`–`:315`), `resolve_addr` for numeric addresses only (`:323`–`:367`), `process_config_files` (`:563`–`:594`), the configuration stage of `main` (`:1039`–`:1612`) |
| | `sshconnect.h` | `DEFAULT_CLIENT_PERCENT_EXPAND_ARGS` (the `%` tokens) |
| `src/misc.rs` | `misc.c` | `strdelim_internal` and `strdelim` (`:481`–`:530`), `a2port` (`:591`–`:604`), `a2tun` (`:606`–`:635`), `convtime_double` and `convtime` (`:670`–`:758`), `hpdelim2` and `hpdelim` (`:822`–`:869`), `cleanhostname` (`:871`–`:879`), `parse_user_host_port` (`:981`–`:1032`), `urldecode` (`:1061`–`:1095`), `parse_uri` (`:1109`–`:1198`), `tilde_expand` and `tilde_expand_filename` (`:1275`–`:1351`), `vdollar_percent_expand`, `dollar_expand`, `percent_expand`, `percent_dollar_expand` (`:1360`–`:1527`), `ipqos[]` (`:1938`), `parse_ipqos` and `iptos2str` (`:1972`–`:2004`), `lowercase` (`:2006`–`:2011`), `argv_split` (`:2142`–`:2209`), `valid_domain` (`:2481`–`:2524`), `valid_env_name` (`:2531`–`:2543`), `atoi_err` (`:2545`–`:2554`), `parse_pattern_interval` (`:2635`–`:2665`), `path_absolute` (`:2668`–`:2672`); `numeric_host` and `numeric_text` are written for this crate from the `getaddrinfo(3)` (`AI_NUMERICHOST`), `inet_aton(3)` and `inet_ntop(3)` manual pages |
| | `defines.h` | `IPTOS_*` values |
| `src/matching.rs` | `match.c` | `match_pattern` (`:81`–`:152`), `match_pattern_list` (`:160`–`:211`), `match_hostname` (`:232`–`:242`), `match_list` (`:312`–`:350`), `filter_list` (`:359`–`:386`), `match_filter_denylist` (`:392`–`:396`), `match_filter_allowlist` (`:402`–`:406`) |
| `src/algorithms.rs` | `cipher.c` | `ciphers[]` names (`:85`), `cipher_alg_list` (`:115`–`:130`), `ciphers_valid` (`:209`–`:232`) |
| | `mac.c` | `macs[]` names (`:58`), `mac_alg_list` (`:83`–`:94`), `mac_valid` (`:235`–`:255`) |
| | `kex-names.c` | `kexalgs[]` names (`:53`), `kex_names_valid` (`:162`–`:182`), `kex_names_cat` (`:200`–`:231`), `kex_assemble_names` (`:240`–`:334`) |
| | `sshkey.c` | `keyimpls[]` names (`:120`), `sshkey_alg_list` (`:322`–`:341`), `sshkey_names_valid2` (`:343`–`:384`) |
| | `myproposal.h` | `KEX_CLIENT_KEX`, `KEX_DEFAULT_PK_ALG`, `KEX_CLIENT_ENCRYPT`, `KEX_CLIENT_MAC`, `SSH_ALLOWED_CA_SIGALGS` |
| `src/settings.rs` | `version.h` | `SSH_RELEASE` |

Nothing that opens a socket, runs a program or does a name service lookup is ported:
`Match exec`, `Match localnetwork` and `CanonicalizeHostname` with a name that is not an
address make the resolution refused instead.


## Tests

`tests/matches_openssh.rs` compares `dump_client_config` with `ssh -F <fixture>/config -G
<target>` of a reference client built from the base commit by
`scripts/install-openssh-ref.sh` (`target/tools/openssh-10.6p2/bin/ssh`, or
`$TASTY_OPENSSH_REF_DIR/bin/ssh`). A case line may end with `=>` and an expected refusal (its
kind and the file and line); the file comment of the test has the form. It runs on Linux and macOS and fails without the client when
`TASTY_OPENSSH_REF_REQUIRED` is set, as in CI; on Windows it does not compare.
