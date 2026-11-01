# Orthanc Portal

A web front-end for an **Orthanc PACS** (recent releases), written in Rust. One website, two doors:

* **Intranet**: the full interface, limited by each user's permissions.
* **Internet** (through a **Cloudflare Tunnel**): **view only**. Search studies and look at them, nothing else.

| Feature | Intranet | Internet |
|---|:---:|:---:|
| Search studies and series, filter by modality (CT, MR, US, RX...), date, patient... | yes | yes |
| View in **Weasis** (desktop viewer) | yes | **no** (it would download the images) |
| View in **OHIF** (in the browser, nothing to install) | yes | **yes, the only viewer** |
| **Preview thumbnails** next to every study and series | yes | yes |
| **Your own branding**: browser window title, website icon (favicon), logo, portal name | set by admins | shown |
| Upload DICOM files / ZIPs | yes | no |
| Download studies / series as ZIP | yes | no |
| **Send a study to a remote DICOM node** and follow the transfer | yes | no |
| User management, audit log | yes | no |
| **Configuration from the web**: Orthanc connection, ports, sessions, upload limit, roles, **DICOM nodes**, portal name | yes (admins) | no |

One self-contained executable: the web UI is compiled into it, users and settings live in a SQLite file,
and the only other thing it needs is Orthanc.

---

## 1. How the pieces fit

```
 Browser on the intranet ──► :8080  ┐
                                    │   ┌────────────────┐  REST + DICOMweb   ┌─────────┐
 Weasis on the intranet  ──────────►│ (+ browser)    │
 Browser (OHIF) on the internet     ├──►│ Orthanc Portal │ ─────────────────► │ Orthanc │
   ──► Cloudflare ──► cloudflared ──► :8081 (view only)  └────────────────┘   │  :8042  │
                                                                               └────┬────┘
                                                     C-STORE (Send to...) ──► remote DICOM nodes
```

* The program listens on **two ports**: `8080` (full, intranet) and `8081` (**view only**, the one the
  Cloudflare Tunnel points to). The upload, download, send and admin routes are not even registered on
  `8081`.
* **Second safeguard:** a request that carries Cloudflare headers (`CF-Connecting-IP`, `CF-Ray`,
  `CDN-Loop: cloudflare`) is treated as remote **even on port 8080**. If someone points the tunnel at
  the wrong port, internet users still only get viewing.
* Orthanc is never exposed: only the portal talks to it, with its own Orthanc account.

---

## 2. Requirements

* **Orthanc 1.12.5 or newer** (the portal uses the recent "extended find": database-side ordering,
  computed tags, list filters). The portal checks `GET /system` at start-up and in **Settings > Test
  connection**; against an older server, or an index without the `HasExtendedFind` capability, it logs an
  error and shows a red notice to administrators (it keeps running, so you can fix the address in Settings).
  There is no compatibility code for old versions. The latest release is recommended.
* Orthanc **DICOMweb plugin** (Weasis and OHIF both need it).
* Orthanc **OHIF plugin**, configured with `"DataSource": "dicom-web"` (optional; without it the
  portal simply hides the browser-viewer button). The official `jodogne/orthanc-plugins` and
  `orthancteam/orthanc` Docker images include these plugins.

---

## 3. Quick start

### Option A: Docker Compose (Orthanc + portal + tunnel)

```bash
cd deploy
# 1. edit orthanc.json: change the password of the "portal" account
#    (remote DICOM nodes are added later from the web interface, see section 8)
# 2. create deploy/.env with:
#      ORTHANC_PORTAL_PASSWORD=<same password as in orthanc.json>
#      ADMIN_PASSWORD=<first portal administrator password, at least 10 characters>
#      TUNNEL_TOKEN=<Cloudflare tunnel token, see section 9>
docker compose up -d --build
```

The image rebuilds quickly after code changes (third-party crates are cached in their own Docker layer) and
reports `healthy` / `unhealthy` through a built-in health check of port 8080.

Open `http://<server>:8080`, sign in as `admin` and open **Settings**: everything else is configured there
(section 4). Without `TUNNEL_TOKEN`, remove the `cloudflared` service from the compose file (intranet only).

### Option B: run the binary directly (you already have an Orthanc)

Requires Rust (stable).

```bash
cargo run --release        # works with no configuration at all
```

If `ADMIN_PASSWORD` is empty, a random password is generated and printed **once** in the log on the first
start (the administrator is only created automatically when the user table is empty). Sign in on
`http://localhost:8080`, open **Settings > Orthanc connection**, enter your Orthanc address and
credentials and press **Test connection**. Optionally copy `.env.example` to `.env` to pre-fill values.

---

## 4. Configuration: from the web interface

Sign in as an **administrator on the intranet** and open **Settings**. Everything applies **immediately**,
without restarting. These screens do not exist on the internet-facing port, and every route behind them
requires the `admin` permission.

| Where | What you can change |
|---|---|
| **Settings > Branding** | **Browser window title**, **website icon**, **logo**, portal name, whether the name is shown next to the logo, notice shown on the login page (see below) |
| **Settings > Orthanc connection** | Address, user name, password (write-only: it is never sent back to the browser), **Test connection** (version, DICOMweb/OHIF plugins, number of DICOM nodes; works on values you have typed but not yet saved) |
| **Settings > Network** | Intranet address, enable/disable the remote (internet, view-only) listener and its address, whether internet viewers may download whole DICOM files (off = allowed; default on = refused), optional fixed public address for Weasis |
| **Settings > Sessions and sign-in security** | Session length, Weasis link validity, failed sign-ins before lockout, lockout length, secure-cookie mode, how long to keep the audit log (0 = forever) |
| **Settings > Viewers and uploads** | OHIF browser viewer (automatic / always / never), largest uploaded file, preview thumbnails on/off and their cache size |
| **Settings > Roles** | The role shortcuts offered in the user form (add, rename, change permissions, restore defaults) |
| **Users** | Accounts, permissions, enable/disable, password reset |
| **DICOM nodes** | Remote DICOM systems Orthanc sends studies to: add, edit, delete, **Test** (DICOM ping) |
| **Audit log** | Every change above is recorded (setting names only, never values or passwords) |

### Branding: make it yours

*Settings > Branding* lets each practice give the portal its own identity. Everything shows **immediately**,
including to people who are already signed in, and to visitors on the internet-facing listener.

| Item | Details |
|---|---|
| **Browser window title** | The text of the tab and window (max 80 characters). Empty = the portal name. |
| **Website icon** | PNG, ICO, SVG, JPEG, WebP or GIF, up to 256 KB; square and at least 64 x 64 px works best. Shown in the tab and in bookmarks. |
| **Logo** | PNG, JPEG, WebP, GIF or SVG, up to 1 MB. Shown in the top bar (about 32 px high, up to 220 px wide) and on the login page. It sits on a **light plate**, so a dark logo stays readable on the dark bar. A wide logo with a transparent background looks best. |
| **Portal name** | Text next to the logo and the login heading. Untick **Show the portal name next to the logo** when the logo already contains the name (it is then kept as the logo's accessible text). |
| **Login notice** | Optional message under the login heading. |

The logo and the website icon are saved **as soon as you choose a file** (they do not wait for *Save settings*);
*Remove* brings back the built-in mark. They are stored in `portal.db`, so they are included in your backups.
The title and the icon are written into the page by the server, so they are right from the first instant
(no flash of the default ones, and they work without JavaScript). Browsers cache website icons stubbornly:
the address changes with every upload, but if a tab still shows the old icon, reload the page or reopen the tab.

**Uploaded images are checked, not trusted.** The file type is decided from the file's own first bytes (not from
its name or what the browser claims). An **SVG** is refused if it contains scripts, event handlers
(`onload=`...), `javascript:` links, `<foreignObject>`, embedded pages or entities. Images are also served with a
`sandbox` Content-Security-Policy, so even a file that slipped through cannot run anything if opened directly.

The remote listener's rule (**view only**) is deliberately *not* a setting: nothing an administrator can type
into the web interface can open upload, download, send or administration to the internet.

**Moving the ports.** Changing an address re-opens the listener while the program runs. The new address is
opened first; if that fails (port in use, invalid address) nothing changes and the reason is shown. If you
move the intranet port, Settings shows a link to the new address, because your current page was opened on the
old one. In Docker leave the addresses alone: the published ports are fixed.

**Start-up values (the only environment variables that matter).**

| Variable | Meaning |
|---|---|
| `DATABASE_PATH` (default `portal.db`) | SQLite file: users, audit log, transfers **and the saved settings** |
| `ADMIN_USERNAME` / `ADMIN_PASSWORD` | First administrator, created only when there are no users |
| `RESET_SETTINGS=true` | **Recovery switch.** For one start, forget the settings saved from the web and go back to the defaults / `.env` values. Users, audit log and transfers are untouched |

Everything else in `.env.example` (`ORTHANC_URL`, `BIND_ADDR`, `SESSION_HOURS`...) is only the **initial value**
of a setting, used until an administrator saves the Settings form; afterwards the saved value wins. This keeps
existing deployments working and means you can run with no `.env` at all.

**If you lock yourself out** (wrong port, wrong address): if the saved intranet address cannot be opened at
start-up, the portal falls back to `0.0.0.0:8080` so you can still sign in and fix it. If you cannot reach it
at all, start once with `RESET_SETTINGS=true`.

> **The Orthanc password is stored in the SQLite file** (it must be usable to log in to Orthanc), exactly like
> it would sit in a `.env`. Protect `portal.db` and its backups accordingly, and use a dedicated, low-privilege
> Orthanc account for the portal.

---

## 5. Users, permissions and access scope

Every user has a set of **permissions**. Every request also has an **access scope**, decided by the server
from where it arrived. What a request may really do is the *intersection* of both.

| Permission | Allows | Works on the internet? |
|---|---|:---:|
| `read` | Search and browse studies and series | yes |
| `visualize` | Open studies in Weasis or OHIF, and see preview thumbnails | yes |
| `upload` | Upload DICOM files / ZIPs | no (intranet only) |
| `download` | Download studies and series as ZIP | no (intranet only) |
| `send` | Send studies to remote DICOM nodes, see transfers | no (intranet only) |
| `admin` | Manage users, read the audit log | no (intranet only) |

So an administrator who signs in through the Cloudflare hostname sees only **Studies** and **Series**, a
"Remote access, view only" badge, and the **View** button. The same account on the intranet sees
everything.

Rules: `visualize`, `download` and `send` require `read`. The last active administrator cannot be removed
or disabled, and nobody can delete or demote their own account.

Ready-made roles in the *Users* screen (they only pre-tick the checkboxes; you can adjust them, and edit the list itself in **Settings > Roles**):

| Role | Permissions |
|---|---|
| Viewer | read, visualize |
| Contributor | read, visualize, upload |
| Exporter | read, visualize, download, send |
| Full clinical | read, visualize, upload, download, send |
| Administrator | everything |

Changes take effect immediately (permissions are read from the database on every request). Disabling a user
or resetting their password also ends their sessions and Weasis tokens.

> **Limit of `download`.** The permission controls the portal's ZIP download. On the intranet anyone who may
> *visualize* can still save images from inside Weasis (that is how viewers work); over the internet Weasis is
> not available at all (section 7). To prevent any export on the intranet too, do not grant `visualize`.

**Adding a permission** (e.g. `delete`): add a variant in `src/permissions.rs` (`ALL`, `as_str`,
`description`, and `AccessScope::allows` to decide whether it may be used remotely), then call
`user.require(Permission::Delete)?` in the new route and register that route inside the
`AccessScope::Full` block of `src/routes/mod.rs`.

---

## 6. Searching and filtering by modality

* **Studies** and **Series** tabs; text fields accept DICOM wildcards (`DOE*`, `?`), a plain word means
  "contains". Study dates are a range. Newest studies come first (sorted by Orthanc, so paging is exact).
* **Modality** is a row of toggle buttons: **CT, MR, US, RX, MG, XA, RF, NM, PT**. Several can be active
  at once (CT *or* US) and the list refreshes immediately. **RX** means X-ray, which DICOM splits into
  `CR` and `DX`, so it selects both. A small box accepts any other code (`SR`, `ES`...).
* Under the hood the codes become a DICOM "any of" list: `ModalitiesInStudy = "CT\CR\DX"`. The API accepts
  `?modality=ct,rx` (case-insensitive, `RX` expands to `CR,DX`).
* The studies table shows a **preview thumbnail**, modality badges, the number of series/images, and expands
  to list each study's series (each with its own thumbnail). The Series tab shows thumbnails too.

### Preview thumbnails

* **Which image?** A series shows its **middle image** (the first and last slices of a CT/MR are often nearly
  empty). A study shows the middle image of its **largest picture series**; series without pixel data (reports,
  presentation states, RT structures...) are skipped, and the next best series is tried if one cannot be decoded.
  Series that have nothing to show get a placeholder with their modality.
* **Cheap for Orthanc.** The portal asks Orthanc for a small JPEG (`/instances/{id}/rendered?width=...`), so a few
  KB travel instead of the full image. At most 4 are produced at once, and the browser only requests the
  thumbnails that scroll into view.
* **Privacy.** A thumbnail is an image of the patient, so it needs the **`visualize`** permission (users who can
  only `read` see the lists without pictures). It is available remotely too, as viewing is. Thumbnails are
  remembered only **in memory** (portal cache + the open page) and are sent with `Cache-Control: no-store`: they
  are never written to the browser's disk cache and are freed when the user signs out.
* **Settings.** *Settings > Viewers and uploads* switches thumbnails on/off for everybody and sets the memory
  cache size (default 64 MB; 0 = no cache). Both apply immediately.

---

## 7. Viewing: Weasis and OHIF

Click **View** on a study (it needs the `visualize` permission).

> **Over the internet only the browser viewer (OHIF) is available.** Weasis saves the images on the user's
> computer, which would let patient images leave the building. So through the Cloudflare hostname the **View**
> button is a single link that opens OHIF in a new tab, and Weasis is blocked by the server, not just hidden:
> its link route does not exist on the remote listener, and a Weasis access token (even a real one issued
> on the intranet) is **ignored** over the internet, where only the browser's session cookie is accepted.
> On the intranet both viewers are offered. If the OHIF plugin is not installed, nothing can be viewed over
> the internet and the page says so.

**OHIF (browser, nothing to install).** Opens `/ohif/viewer?StudyInstanceUIDs=<uid>` in a new tab. The
portal relays Orthanc's OHIF plugin and the DICOMweb data it reads. OHIF runs inside the portal's own
origin, so it authenticates with the user's session cookie. Only the OHIF pages get a more permissive
Content-Security-Policy (OHIF needs WebAssembly decoders, web workers and inline styles); the rest of the
portal keeps a strict one.

**Weasis (desktop).** Install [Weasis](https://weasis.org) on the workstation (its installers register the
`weasis://` link handler). The portal creates a short-lived token and a link of this form (shown decoded):

```
weasis://$dicom:rs --url "https://pacs.example.com/dicom-web" -r "studyUID=1.2.840..." -H "Authorization: Bearer <token>"
```

Weasis then reads `/dicom-web/...` on the portal with that token. The address inside the link is the one
the user is currently using, so the same button works from the intranet (`http://10.0.0.5:8080`) and from
the internet (`https://pacs.example.com`). It opens the **whole study**.

**The read-only proxy.** `/dicom-web/*`, `/wado` and `/ohif/*` are forwarded to Orthanc with **GET only**,
after checking the token/cookie and the `visualize` permission. Paths are validated (no `..`), the user's
cookie and token are never sent to Orthanc, and patient data is marked non-cacheable.

**No whole DICOM files over the internet.** The viewer's own address could otherwise be used to download
`.dcm` files with the session cookie (`/dicom-web/studies/.../instances/...`). Over the internet the portal
therefore refuses the requests that return **entire DICOM files** (a whole study, series or instance, and
WADO-URI unless a picture format such as `image/jpeg` is requested) and only relays what a viewer needs:
searches, metadata, pixel frames, rendered pictures and thumbnails. The intranet is not restricted (Weasis needs
whole files). It is the setting *Settings > Network > Internet viewers cannot download whole DICOM files*,
**on by default**; switch it off only if some special object type (for example a segmentation or an RT object)
fails to display over the internet. Weasis stays blocked remotely either way.

**What this does not do.** OHIF has to receive the pixel data to draw it, so the images do reach the remote
browser's memory, and the viewer has its own screenshot tools. The measures above stop *casual* leakage (no
downloaded files, no desktop viewer cache); they cannot stop a determined person who is allowed to look at an
image from copying what is on their screen. For stricter needs combine them with Cloudflare Access (named
accounts, short sessions), short portal sessions (*Settings*), and the audit log.

---

## 8. Sending a study to a remote DICOM node

Orthanc calls the remote systems it can talk to **DICOM modalities**. Administrators manage them in the
portal's **DICOM nodes** tab: *Add node* (name, AE title, host, port), *Edit*, *Delete*, and **Test** (a DICOM
ping, C-ECHO). The portal stores them in Orthanc itself through its REST API; they survive an Orthanc restart
only if Orthanc runs with `"DicomModalitiesInDatabase": true` (set in `deploy/orthanc.json`). You can also keep
declaring them in Orthanc's `DicomModalities`. Do not forget to declare Orthanc's AE title and address on the
other side.

1. On the **Studies** tab (intranet), click **Send to...** on a study, pick the destination, **Send study**.
2. The portal asks Orthanc to run a C-STORE job (`POST /modalities/{alias}/store` with
   `"Synchronous": false`), stores the job id and answers immediately.
3. The **Transfers** tab shows each transfer with the live Orthanc job state
   (Pending / Running / Success / Failure) and progress, refreshing every 3 seconds while a job is active.
   Users see their own transfers; administrators see everyone's.

Needs the `send` permission. Destinations with `"AllowStore": false` are not offered. Every transfer is
written to the audit log.

---

## 9. Cloudflare Tunnel

The tunnel makes an **outbound** connection from your server to Cloudflare: no port forwarding, and
Cloudflare provides HTTPS.

1. Cloudflare dashboard, **Zero Trust**, **Networks**, **Tunnels**: create a tunnel (type *Cloudflared*).
2. Copy its **token** into `TUNNEL_TOKEN` (docker-compose), or follow `deploy/cloudflared-config.yml` for a
   locally-managed tunnel running on the host.
3. Add a **public hostname** (e.g. `pacs.example.com`) with service **HTTP** pointing to the **view-only
   port**: `portal:8081` (`localhost:8081` when cloudflared runs on the host). **Not 8080.**
4. Browse to `https://pacs.example.com`. Intranet users keep using `http://<server>:8080`.

Things to know:

* **Never publish port 8080 to the internet.** Even if you did, Cloudflare headers would downgrade those
  requests to view-only, but the correct setup is to expose only 8081 (docker-compose does this).
* **Cloudflare Access (optional extra login layer).** Over the internet only the browser viewer is used, so
  Access works like on any other web page: no bypass rule is needed (Weasis, which could not complete an
  Access login, is not available remotely anyway).
* **Large images through the tunnel.** Streaming multi-hundred-MB studies to a remote viewer works, but a
  slow internet link is the limit; the intranet is faster for big CT/MR series.
* Consider a Cloudflare rate-limiting rule on `/api/login` as a second layer against password guessing.

---

## 10. Security summary

Built in:

* **Intranet/internet separation** by listener *and* by Cloudflare headers (section 1); remote sessions can
  never use upload, download, send or admin, whatever the account.
* Passwords hashed with **Argon2**; sign-in takes the same time for unknown users (no user enumeration).
* Temporary lock after repeated failed logins, **per account and per address**: a single address that fails
  4 times more often than the per-account limit is locked too, so it cannot try thousands of different names
  (the defaults are 5 per account / 20 per address, 5 minutes; both follow the Settings).
* Login input is checked before anything is hashed, stored or logged: user names are 1-64 characters from a
  small alphabet, passwords at most 1024. Password hashing (Argon2 uses ~19 MB of memory each) runs at most 4 at
  a time, so a flood of logins cannot exhaust the server's memory.
* At most 20 simultaneous sessions per user (the oldest are dropped); Weasis links reuse the user's current
  token instead of creating a new one per click.
* The DICOMweb proxy relays only `studies`, `series` and `instances`. The plugin's other resources (such as
  queries to *other* DICOMweb servers configured in Orthanc) are not reachable through the portal.
* Every answer carries `Permissions-Policy`; JSON answers are `no-store`; HSTS is sent when the portal is
  reached over HTTPS. `portal.db` is created readable by its owner only (mode 600).
* Administrators' changes are serialised, so two of them working at the same instant cannot overwrite each
  other's settings, or both remove the last administrators.
* Session cookie `HttpOnly`, `SameSite=Lax`, `Secure` automatically over HTTPS.
* **CSRF** protection: every POST/PUT/DELETE must carry `X-Requested-With: portal`, which cross-site pages
  cannot add.
* Strict `Content-Security-Policy` (no inline scripts, no external resources) except on the OHIF pages,
  `X-Frame-Options: DENY`.
* DICOM text is only ever inserted into the page as text, never as HTML.
* Orthanc ids, DICOM UIDs, modality codes, DICOM destinations and proxy paths are validated; a destination
  is only ever *picked* from Orthanc's own list, never built from user text.
* Viewer proxy is GET-only; the user's credentials never reach Orthanc.
* **Configuration is admin-only and intranet-only**: the settings and DICOM-node routes require the `admin`
  permission and are not registered on the internet-facing port (nor reachable with Cloudflare headers).
* Uploaded logo/icon files are identified by content, size-limited, and SVGs must be free of scripts; they are
  served sandboxed. Only administrators on the intranet can change them.
* Settings are validated before they are saved; the Orthanc password is write-only and masked in logs; a new
  listener address is opened before the old one is released, so a typo cannot take the portal down.
* Audit log (logins with IP and scope, uploads, downloads, sends, Weasis launches, user changes, **settings and
  DICOM-node changes**).

Operational advice:

* Change every default password; use long passwords for `admin`.
* **Back up** the `portal-data` volume (users, audit, transfers) and the Orthanc volume (images).
* Do not publish Orthanc's port 8042; keep the DICOM port 4242 on the intranet.
* `portal.db` holds the Orthanc password: restrict access to it and to its backups.
* Sessions and tokens live in memory: restarting the portal signs everybody out.
* The audit log grows over time: set *Keep the audit log for (days)* in Settings if your policy allows deleting
  old entries (the default keeps everything).
* There is no per-study access control: every user with `read` sees every study.
* OHIF is third-party code served from the portal's origin; keep Orthanc and its plugins up to date.
* Medical data is regulated (GDPR/HIPAA...). This tool gives you the controls; policies and risk assessment
  remain your responsibility.

---

## 11. API reference

All JSON, cookie-authenticated. State-changing calls need the header `X-Requested-With: portal`.
"intranet" = only registered on the full listener.

| Method & path | Permission | Purpose |
|---|---|---|
| `POST /api/login`, `POST /api/logout`, `GET /api/me` | none / session | Session (`/me` returns effective permissions, `scope`, `features.ohif`) |
| `GET /api/studies?patient_name=&patient_id=&accession_number=&description=&modality=&date_from=&date_to=&limit=&offset=` | read | Search studies (`modality=ct,rx`) |
| `GET /api/studies/{id}/series` | read | Series of a study |
| `GET /api/series?...&body_part=` | read | Search series |
| `GET /api/studies/{id}/thumbnail`, `GET /api/series/{id}/thumbnail` (`?size=64\|128\|256`) | visualize | Preview JPEG (404 = nothing to show) |
| `GET /api/weasis/link?study_uid=` | visualize, **intranet only** | Build the `weasis://` link (Weasis is never offered over the internet) |
| `GET /dicom-web/*`, `GET /wado`, `GET /ohif/*` | visualize (session cookie; bearer token **on the intranet only**) | Read-only viewer proxy; over the internet no whole DICOM files |
| `POST /api/upload` (raw body) | upload, intranet | Upload one file |
| `GET /api/studies/{id}/download[?dicomdir=true]` | download, intranet | ZIP of a study |
| `GET /api/series/{id}/download[?dicomdir=true]` | download, intranet | ZIP of a series |
| `GET /api/modalities` | send, intranet | DICOM destinations |
| `POST /api/studies/{id}/send` `{ "target": "alias" }` | send, intranet | Start sending a study |
| `GET /api/transfers` | send, intranet | Recent transfers + live job state |
| `GET/POST /api/admin/users`, `PUT/DELETE /api/admin/users/{id}`, `GET /api/admin/audit` | admin, intranet | Users and audit log |
| `GET /api/public` | none | Branding: name, window title, logo, icon, login notice |
| `GET /branding/logo`, `GET /branding/favicon` | none | The uploaded logo / website icon (or the built-in mark) |
| `PUT/DELETE /api/admin/branding/{logo\|favicon}` (raw file body) | admin, intranet | Upload / remove the logo or the website icon |
| `GET/PUT /api/admin/settings` | admin, intranet | Read / save the settings (partial update; Orthanc password write-only) |
| `POST /api/admin/orthanc/test` | admin, intranet | "Test connection" (saved or typed values) |
| `GET /api/admin/nodes`, `PUT/DELETE /api/admin/nodes/{name}`, `POST /api/admin/nodes/{name}/echo` | admin, intranet | Manage DICOM nodes, DICOM ping |
| `GET /healthz` | none | Liveness probe |

---

## 12. Project layout

```
Cargo.toml            dependencies
.env.example          every setting, documented
Dockerfile            container image
deploy/               docker-compose.yml, orthanc.json, cloudflared-config.yml
static/               web UI: index.html (a template: title and icon are filled in per request), app.css, app.js, favicon.svg (the default mark); all embedded in the binary
src/main.rs           start-up, background cleanup, graceful shutdown
src/config.rs         the few bootstrap values from the environment
src/settings.rs       every web-editable setting: defaults, validation, saved copy, roles
src/server.rs         runs / re-opens the two listeners while the program runs
src/error.rs          AppError -> HTTP status + JSON
src/permissions.rs    permissions, intranet/internet access scope, role presets
src/db.rs             SQLite: users, audit log, transfers, saved settings
src/auth.rs           passwords, sessions, tokens, scope detection, request guards
src/branding.rs       uploaded logo / website icon: content checks, SVG safety, memory copy
src/thumbnails.rs     chooses the preview image of a study/series, memory cache
src/orthanc.rs        Orthanc REST client (find, upload, archive, nodes/jobs, viewer proxy), reconfigurable live
src/models.rs         study / series / DICOM-node JSON shapes sent to the browser
src/util.rs           validation and helpers
src/state.rs          shared application state
src/routes/           session, studies, thumbnails, upload, download, send, viewer (Weasis + proxy), admin, settings, branding, router
```

---

## 13. Testing status, first-run checklist, troubleshooting

The code compiles without warnings on Rust 1.91, passes `cargo clippy --all-targets` with no warnings, is
formatted with `rustfmt` (see `rustfmt.toml`), and its dependencies were checked against the RustSec advisory
database (no known vulnerable version in `Cargo.lock`). The automated tests live in the **`tests/`** folder:

* `tests/server_tests.sh`: **136 checks** of the server against a fake Orthanc (`tests/mock_orthanc.py`): both
  access scopes, the **OHIF-only rule over the internet** (Weasis link, Weasis tokens, whole-file requests),
  search and modality filters, thumbnails, sending and job tracking, upload/download, branding including
  malicious files, live settings, login hardening, caps, ETag/headers, concurrent administrators;
* `tests/ui_tests.js`: **25 checks** of the web UI in a simulated browser (jsdom): what each kind of user sees,
  View button over the intranet vs the internet, the new setting, search;
* `cargo test`: unit tests of the image validation.

Run them all with `tests/run_all.sh` (needs `python3`, `curl`, and `npm install` once inside `tests/`; build
first with `cargo build`; set `PORTAL_BIN=target/release/orthanc-portal` to test the release binary). The
portal has **not** been run against a real Orthanc, a real OHIF
plugin, a real Weasis or a real DICOM destination here, so on first use please verify:

0. **Settings > Orthanc connection > Test connection** shows your Orthanc version, the DICOMweb and OHIF
   plugins and the number of DICOM nodes.
1. **Modality filter**: select CT and RX; results include only those modalities.
2. **Browser viewer**: *View, Browser viewer (OHIF)* loads images (needs the OHIF plugin with
   `"DataSource": "dicom-web"`).
3. **Weasis**: *View, Weasis* loads the study (Weasis installed, DICOMweb plugin enabled, portal address
   reachable from the workstation).
4. **DICOM nodes**: add your real node, press **Test** (it must answer), then use **Send to...** on a small study
   and watch it reach *Success* in **Transfers**.
5. **From the internet**: through the Cloudflare hostname you see only Studies and Series.
6. **Thumbnails**: the Studies and Series tabs show pictures. Not every image can be rendered (for example some
   JPEG 2000 files depending on how your Orthanc was built); those get the modality placeholder.

| Symptom | Likely cause |
|---|---|
| Red "Orthanc:" strip under the top bar, or "Orthanc ... is not supported" in the log | Orthanc unreachable, wrong credentials, or older than 1.12.5 / no `HasExtendedFind`. Fix it in **Settings > Orthanc connection**; the portal keeps running meanwhile |
| I changed the port and lost the page | Settings shows a link to the new address; if the new address is unusable the old one is kept. Last resort: start once with `RESET_SETTINGS=true` |
| A DICOM node I added disappears after restarting Orthanc | Orthanc is not running with `"DicomModalitiesInDatabase": true` |
| Saving settings says "Cannot listen on ..." | That address is already used by another program or is not an address of this machine |
| "The Orthanc server is unreachable" | Wrong `ORTHANC_URL`, Orthanc down, or firewall between them |
| "PACS returned an error: HTTP 401" | Wrong `ORTHANC_USERNAME` / `ORTHANC_PASSWORD` |
| Search error mentioning `Limit`/`Since` or a tag | Orthanc refused that filter for your index; the message comes from Orthanc |
| No "Browser viewer" entry | OHIF plugin not loaded (see `GET /plugins` on Orthanc) or `OHIF_ENABLED=false` |
| OHIF opens but shows no images / 404 on `/studies/...` | The plugin uses its default `dicom-json` data source: set `"OHIF": {"DataSource": "dicom-web"}` |
| OHIF page stays blank | Check the browser console for blocked resources; OHIF pages use the relaxed policy in `security_headers` (`src/routes/mod.rs`) |
| Weasis starts but shows no images | DICOMweb plugin disabled; or the workstation cannot reach the portal address in the link; or the token expired (click View again) |
| "Send to..." says no destination | No DICOM node yet (add one in the **DICOM nodes** tab), or all have *Send* blocked |
| A transfer ends in Failure | Orthanc's explanation is shown under the state; usually the remote node does not know Orthanc's AET or is unreachable (try a DICOM echo from Orthanc) |
| Internet users see the full interface | The tunnel points to 8080: change it to 8081 |
| Every thumbnail is a grey placeholder | Thumbnails switched off in Settings, the user lacks `visualize`, or Orthanc cannot render those images (check `GET /instances/{id}/rendered` on Orthanc) |
| Thumbnails are slow on the first view of a big study | Orthanc is decoding images; they load as you scroll and are then cached in memory (raise *Thumbnail cache* if you browse many studies) |
| The tab still shows the old icon after I changed it | Browsers cache favicons aggressively: reload the page, or close and reopen the tab |
| My logo is refused | Only PNG, JPEG, WebP, GIF or SVG up to 1 MB (icon: up to 256 KB). An SVG with scripts, event handlers or `<foreignObject>` is refused: export a plain SVG |
| My logo looks too small or cut | It is shown about 32 px high (48 px on the login page) and at most 220 px wide: use a wide image with a transparent background |
| "Too many failed attempts" for everybody in one office | The limit per *address* was reached (4 times the per-account limit). It clears after the lockout time; raise the numbers in *Settings > Sessions and sign-in security* if your staff share one address |
| Login works on the intranet but not through Cloudflare (or the reverse) | `COOKIE_SECURE=true` over plain HTTP; keep `auto` |

---

## 14. Maintenance

Before publishing a change: `cargo fmt`, `cargo clippy --all-targets`, `cargo test`, `cargo build --release`.
Check the dependencies for known vulnerabilities regularly (for example monthly) with
[`cargo audit`](https://crates.io/crates/cargo-audit) and update with `cargo update` (patch-level updates are
compatible). The portal itself needs no maintenance task: sessions, tokens and old audit records are cleaned up
automatically (audit retention is a setting).

Known limits, by design: Orthanc credentials are stored in `portal.db` in clear (the portal must use them);
there is no per-study access control; sessions live in memory; the portal writes no access log of its own (the
audit log records sensitive actions only). Two dependency upgrades were deliberately left for a later,
separate step because they are major versions with API changes: `reqwest` 0.12 -> 0.13 and `argon2` 0.5 -> 0.6.
