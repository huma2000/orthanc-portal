# Orthanc Portal

A web front-end for an **Orthanc PACS** (recent releases), written in Rust. One website, two doors:

* **Intranet**: the whole portal for your staff (accounts, each limited by their permissions).
* **Internet** (through a **Cloudflare Tunnel**): **by invitation only**. Nobody outside has an account. A staff
  member **shares one study**: the portal creates a **link** and a generated **password**. The guest opens the
  link, types the password, and sees that one study: its details and series, a button to **view it online**
  (OHIF) and a button to **download it as a ZIP**, with the **time left** and the **downloads left** shown.

| Feature | Intranet (staff) | Internet (guest with an invitation) |
|---|:---:|:---:|
| Search studies and series, filter by modality (CT, MR, US, RX...), date, patient... | yes | no |
| View in **Weasis** (desktop viewer) | yes | **no** (it would download the images) |
| View in **OHIF** (in the browser, nothing to install) | yes | yes, **only the shared study** |
| **Preview thumbnails** next to every study and series | yes | no |
| **Share a study**: link + password, valid 48 hours by default, 1 download by default (both adjustable) | yes (`share`) | - |
| Download as ZIP | yes (`download`) | only the shared study, **as many times as the link allows** |
| **Your own branding**: browser window title, website icon (favicon), logo, portal name | set by admins | shown on the invitation page |
| Upload DICOM files / ZIPs | yes | no |
| **Send a study to a remote DICOM node** and follow the transfer | yes | no |
| User management, audit log | yes | no |
| **Configuration from the web**: Orthanc connection, ports, share links, sessions, upload limit, roles, **DICOM nodes**, portal name | yes (admins) | no |
| Talks to Orthanc **with an account or without one** (an Orthanc that only accepts a list of allowed computers) | choose in Settings | - |

One self-contained executable: the web UI is compiled into it, users and settings live in a SQLite file,
and the only other thing it needs is Orthanc (yours: **three ways to install it**, see section 3).

---

## 1. How the pieces fit

```
 Staff browser (intranet) ──► :8080 ┐
 Weasis (intranet)        ──────────┤   ┌────────────────┐  REST + DICOMweb   ┌─────────┐
                                    ├──►│ Orthanc Portal │ ─────────────────► │ Orthanc │
 Guest with an invitation link      │   └────────────────┘                    │  :8042  │
   ──► Cloudflare ──► cloudflared ──► :8081 (invitation links only)           └────┬────┘
                                                     C-STORE (Send to...) ──► remote DICOM nodes
```

* The program listens on **two ports**: `8080` (the staff portal, intranet) and `8081` (**invitation links
  only**, the one the Cloudflare Tunnel points to). The login, search, upload, send, share and admin routes
  are not even registered on `8081`: there is nothing to log in to.
* **Second safeguard:** a request that carries Cloudflare headers (`CF-Connecting-IP`, `CF-Ray`,
  `CDN-Loop: cloudflare`) is **refused on port 8080** (403). If someone points the tunnel at the wrong port,
  the internet gets an error, not the staff portal.
* Orthanc is never exposed: only the portal talks to it, **with its own Orthanc account, or without any
  account** when your Orthanc has no users and only accepts a list of allowed computers (section 3).

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

## 3. Quick start: three ways to install it

| | Method | Where | What it starts | Choose it when |
|:-:|---|---|---|---|
| **A** | **Portal + Cloudflare** (Docker Compose) | `deploy/existing-orthanc/` | the portal and the Cloudflare Tunnel | you **already have an Orthanc**. Intranet and internet work out of the box |
| **B** | **Everything** (Docker Compose) | `deploy/` | a **preconfigured Orthanc**, the portal and the Cloudflare Tunnel | you have **no Orthanc yet**. Intranet and internet work out of the box |
| **C** | **Bare metal** (the executable) | the project root | the portal only | you do not use Docker. The intranet works at once; for the internet you install `cloudflared` yourself (section 10) |

In A and B the tunnel needs a **Cloudflare tunnel token** (section 10). Without it, remove the `cloudflared`
service from the compose file and you have an intranet-only install.

### Option A: Docker Compose, portal + Cloudflare (you already have an Orthanc)

For a server that **already has an Orthanc** (on this computer, in another container or on another machine of
the network). `deploy/existing-orthanc/` starts the **portal and the Cloudflare Tunnel**, but not Orthanc.

```bash
cd deploy/existing-orthanc
cp .env.example .env            # edit it: ORTHANC_URL and TUNNEL_TOKEN
docker compose up -d --build
docker compose logs portal      # prints the first administrator's password if ADMIN_PASSWORD was left empty
```

1. **`ORTHANC_URL`**, for example `http://192.168.1.50:8042`. Use `http://host.docker.internal:8042` for an
   Orthanc on this same computer, or join the network of the container that runs Orthanc (the compose file
   explains each case at the top).
2. **`ORTHANC_USERNAME` / `ORTHANC_PASSWORD`** are optional: **leave both empty when your Orthanc has no users**
   and only accepts a list of allowed computers (see *Talking to Orthanc* below).
3. **`TUNNEL_TOKEN`**: the token of your Cloudflare tunnel. In the Cloudflare dashboard add a public hostname
   with service **HTTP `portal:8081`** (section 10), and type that public address in the portal under
   *Settings > Share links*.
4. Open `http://<server>:8080`, sign in as `admin` and press **Test connection** in *Settings > Orthanc
   connection*: it says whether Orthanc accepts the portal and, if not, why.

Things to know:

* The staff portal is published on `8080` and the invitation links use `8081`. If another program already
  uses one of them, change `INTRANET_PORT` / `INTERNET_PORT` (and `INTRANET_BIND`) in `.env` and run
  `docker compose up -d` (see [Changing the ports](#changing-the-ports)). The invitation-links port is **not
  published**: only the tunnel container reaches it over the Docker network. The intranet port must never be
  forwarded to the internet.
* The portal container is hardened (read-only file system, no Linux capabilities, no privilege escalation); the
  only thing it writes is its database, in the `portal-data` volume (**back it up**: users, audit log, settings).
* Apart from the ports, the values in `.env` are only the **initial** ones. After the first start, change them
  in the browser (*Settings*). Editing `.env` later does not change a portal that already started; start once
  with `RESET_SETTINGS=true` if you want to go back to the values of the file. The ports are the exception:
  they are applied on every start and are read-only in *Settings > Network*.
* The image rebuilds quickly after code changes (third-party crates are cached in their own Docker layer) and
  reports `healthy` / `unhealthy` through a built-in health check of the intranet port.
* Updating: `git pull`, then `docker compose up -d --build` again. Users and settings are kept.

### Option B: Docker Compose, everything (Orthanc + portal + Cloudflare)

For a server **without an Orthanc yet**. `deploy/` starts a **preconfigured Orthanc**, the portal and the
Cloudflare Tunnel.

```bash
cd deploy
# 1. edit orthanc.json: change the password of the "portal" account under "RegisteredUsers"
cp .env.example .env
# 2. edit .env:
#      ORTHANC_PORTAL_PASSWORD=<the same password as in orthanc.json>
#      TUNNEL_TOKEN=<Cloudflare tunnel token, see section 10>
#      ADMIN_PASSWORD=<first portal administrator password, at least 10 characters (or empty: printed in the log)>
docker compose up -d --build
```

What the Orthanc is already set up with (`deploy/orthanc.json`, official `jodogne/orthanc-plugins` image):
the account the portal uses, the **DICOMweb** plugin and the **OHIF** plugin with the `dicom-web` data source,
DICOM nodes kept in its database (so the ones you add in the portal survive a restart), DICOM port `4242`
published for your scanners (`DICOM_PORT` in `.env`; remove that line if nothing sends images by DICOM) and its web port `8042`
**not published**: only the portal can reach it. Images and index live in the `orthanc-db` volume (**back it
up**). Pin a version tag in the compose file instead of `latest` for production.

Then follow steps 3 and 4 of option A (public hostname `portal:8081`, public address in the portal, **Test
connection**). Updating the portal: `git pull` and `docker compose up -d --build`.

### Option C: bare metal, the executable (you already have an Orthanc)

Requires Rust (stable).

```bash
cargo run --release        # works with no configuration at all
```

If `ADMIN_PASSWORD` is empty, a random password is generated and printed **once** in the log on the first
start (the administrator is only created automatically when the user table is empty). Sign in on
`http://localhost:8080`, open **Settings > Orthanc connection**, enter your Orthanc address and, if it has
users, the account, then press **Test connection**. Optionally copy `.env.example` to `.env` to pre-fill values.
If `8080` or `8081` is already used on this computer, set `BIND_ADDR` / `REMOTE_BIND_ADDR` in `.env` before the
first start (see [Changing the ports](#changing-the-ports)).

For the internet, install `cloudflared` on the same server and point the tunnel's public hostname to
`http://localhost:8081` (section 10; `deploy/cloudflared-config.yml` is a ready-made example for a
locally-managed tunnel).

### Talking to Orthanc: with or without an account

Orthanc protects its REST API in one of two ways, and the portal works with both. You choose in
**Settings > Orthanc connection > How does Orthanc let the portal in?**

| Your Orthanc | Choose | What the portal does |
|---|---|---|
| **Has users** (`"AuthenticationEnabled": true` and `"RegisteredUsers"`) | **With an account** | Sends the user name and password (HTTP Basic) with every request |
| **Has no users** and only lets certain computers in (a firewall, a reverse proxy with `allow` / `deny`, or an `IncomingHttpRequestFilter` that checks the client address) | **Without an account** | Sends **no credentials at all**: the portal's computer has to be one of the allowed ones |

* "Without an account" is stored as an empty user name, and the saved password is erased. In the environment
  (first start only) it means leaving `ORTHANC_USERNAME` and `ORTHANC_PASSWORD` empty.
* An account that Orthanc does not need is harmless: Orthanc ignores the credentials.
* **Test connection** tells you which mode it used (*Connected to Orthanc 1.12.x without an account*) and, when
  Orthanc says no, why: a wrong account is reported as such; a refusal without an account says that Orthanc
  *refuses this machine* and that its allowed list must include the portal's address.
* Orthanc itself has no HTTP address list: it is done in front of it (firewall, reverse proxy) or with the
  `IncomingHttpRequestFilter` callback, which receives the client's address. Whatever you use, the question is
  the same: **which address does Orthanc see?**

| Where the portal runs | Orthanc is... | Orthanc sees the portal as |
|---|---|---|
| Directly on the server (option C) | on the same computer | `127.0.0.1` |
| Directly or in Docker | on another computer | this server's LAN address (Docker translates the container's address) |
| Docker | on this same computer | an address of Docker's private network (`172.x.x.x`), **not** `127.0.0.1` |
| Docker, joined to the network of Orthanc's container | in another container | the portal container's address on that network (`172.x.x.x`) |
| Docker with `network_mode: host` (Linux) | on the same computer | `127.0.0.1` or the server's own address |

When the allowed list is the problem, the simplest fixes are to allow the whole Docker range (for example
`172.16.0.0/12`) or the server's address, or to run the portal with `network_mode: host`. The log of Orthanc
(or of the reverse proxy in front of it) shows the address it actually sees.

---

## 4. Configuration: from the web interface

Sign in as an **administrator on the intranet** and open **Settings**. Everything applies **immediately**,
without restarting. These screens do not exist on the internet-facing port, and every route behind them
requires the `admin` permission.

Settings is organised in **tabs**. The first four are one form, saved together with **Save settings** (values
typed in one tab are kept while you visit another, until you leave Settings). The last three are separate
screens whose changes apply at once.

| Tab > section | What you can change |
|---|---|
| **General > Branding** | **Browser window title**, **website icon**, **logo**, portal name, whether the name is shown next to the logo, notice shown on the login page (see below) |
| **General > Viewers and uploads** | OHIF browser viewer (automatic / always / never), largest uploaded file, preview thumbnails on/off and their cache size |
| **Orthanc > Orthanc connection** | Address; **with an account** (user name + password, write-only: never sent back to the browser) **or without an account** (for an Orthanc that only accepts a list of allowed computers); **Test connection** (mode used, version, DICOMweb/OHIF plugins, number of DICOM nodes; works on values you have typed but not yet saved) |
| **Network & sharing > Network** | Intranet address, enable/disable the internet listener (invitation links only) and its address, optional fixed public address for Weasis |
| **Network & sharing > Share links** | **The public address of the share links** (your Cloudflare hostname; sharing stays off until it is set), the default and maximum validity (hours) and the default and maximum number of ZIP downloads |
| **Security & roles > Sessions and sign-in security** | Session length, Weasis link validity, failed sign-ins before lockout, lockout length, secure-cookie mode, how long to keep the audit log (0 = forever) |
| **Security & roles > Roles** | The role shortcuts offered in the user form (add, rename, change permissions, restore defaults) |
| **Users** | Accounts, permissions, enable/disable, password reset |
| **DICOM nodes** | Remote DICOM systems Orthanc sends studies to: add, edit, delete, **Test** (DICOM ping) |
| **Audit log** | Every change above is recorded (setting names only, never values or passwords) |

In the rest of this document *Settings > Share links* means the **Share links** section (here in the
*Network & sharing* tab), and so on. The red "Orthanc" notice and the *Share...* dialog open the right tab.

### Branding: make it yours

*Settings > Branding* lets each practice give the portal its own identity. Everything shows **immediately**,
including to people who are already signed in, and on the invitation page that guests see.

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

What the internet listener serves (**invitation links only**) is deliberately *not* a setting: nothing an
administrator can type into the web interface can open search, upload, send or administration to the internet.
The settings only decide *whether* sharing is on and *how generous* a link may be.

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
| `LISTENERS_FROM_ENV=true` | The listening addresses always come from `BIND_ADDR` / `REMOTE_BIND_ADDR` and are read-only in *Settings > Network* (always on in the Docker image). See [Changing the ports](#changing-the-ports) |

Everything else in `.env.example` (`ORTHANC_URL`, `BIND_ADDR`, `SESSION_HOURS`...) is only the **initial value**
of a setting, used until an administrator saves the Settings form; afterwards the saved value wins. This keeps
existing deployments working and means you can run with no `.env` at all.

**If you lock yourself out** (wrong port, wrong address): if the saved intranet address cannot be opened at
start-up, the portal falls back to `BIND_ADDR` (default `0.0.0.0:8080`) so you can still sign in and fix it.
If you cannot reach it at all, start once with `RESET_SETTINGS=true`.

### Changing the ports

The defaults are `8080` (intranet, staff portal) and `8081` (internet, invitation links only). If another
program already uses one of them:

| Install | Where to change them | When it applies |
|---|---|---|
| Docker Compose (all three files in `deploy/`) | `INTRANET_PORT` / `INTERNET_PORT` in the `.env` next to `docker-compose.yml` (`DICOM_PORT` too in the full stack) | `docker compose up -d`. Applied on **every** start; the address fields in *Settings > Network* are read-only |
| Bare metal, first start | `BIND_ADDR` / `REMOTE_BIND_ADDR` in `.env` (e.g. `0.0.0.0:9080` / `0.0.0.0:9081`) | First start; afterwards the saved value wins |
| Bare metal, running portal | *Settings > Network* | At once, no restart (if the new port cannot be opened, nothing changes) |
| Bare metal, ports managed in `.env` only | `LISTENERS_FROM_ENV=true` plus `BIND_ADDR` / `REMOTE_BIND_ADDR` | Every start, like Docker |

In Docker the container listens on the **same** port numbers as the server, so the addresses shown in
*Settings* are the real ones, and `network_mode: host` (option D in the compose files) works unchanged. The
image sets `LISTENERS_FROM_ENV=true`, so a port typed in *Settings* can never break the port mapping.

When you change the **internet** port, update the Cloudflare tunnel to match: `portal:<INTERNET_PORT>` for the
tunnel container, `localhost:<port>` for a `cloudflared` running on the server.

> **The Orthanc password is stored in the SQLite file** (it must be usable to log in to Orthanc), exactly like
> it would sit in a `.env`. Protect `portal.db` and its backups accordingly, and use a dedicated, low-privilege
> Orthanc account for the portal.

---

## 5. Users and permissions

Only staff have accounts, and they sign in on the intranet. Every user has a set of **permissions**; guests
with an invitation have no account and no permissions, only their link (section 8).

| Permission | Allows |
|---|---|
| `read` | Search and browse studies and series |
| `visualize` | Open studies in Weasis or OHIF, and see preview thumbnails |
| `upload` | Upload DICOM files / ZIPs |
| `download` | Download studies and series as ZIP |
| `send` | Send studies to remote DICOM nodes, see transfers |
| `share` | **Invite somebody outside to look at a study** (create links), see and revoke their own links |
| `admin` | Manage users, settings, DICOM nodes, branding, read the audit log; see and revoke **everyone's** links |

Rules: `visualize`, `download`, `send` and `share` require `read`. The last active administrator cannot be
removed or disabled, and nobody can delete or demote their own account.

Ready-made roles in the *Settings > Users* screen (they only pre-tick the checkboxes; you can adjust them, and edit the list itself in **Settings > Roles**):

| Role | Permissions |
|---|---|
| Viewer | read, visualize |
| Contributor | read, visualize, upload |
| Exporter | read, visualize, download, send, share |
| Full clinical | read, visualize, upload, download, send, share |
| Administrator | everything |

Changes take effect immediately (permissions are read from the database on every request). Disabling a user
or resetting their password also ends their sessions and Weasis tokens. An administrator who changes their
**own** password keeps the session they are using; every other session of that account is signed out.

> **Limit of `download`.** The permission controls the portal's ZIP download. Anyone on the intranet who may
> *visualize* can still save images from inside Weasis (that is how viewers work). To prevent any export on the
> intranet too, do not grant `visualize`. Note that `share` lets a person hand a study to somebody outside
> (limited by the number of downloads you allow), so grant it as you would `download`.

**Adding a permission** (e.g. `delete`): add a variant in `src/permissions.rs` (`ALL`, `as_str`,
`description`), then call `user.require(Permission::Delete)?` in the new route and register that route in
`staff_router` of `src/routes/mod.rs`.

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
  only `read` see the lists without pictures). They exist only on the intranet. Thumbnails are
  remembered only **in memory** (portal cache + the open page) and are sent with `Cache-Control: no-store`: they
  are never written to the browser's disk cache and are freed when the user signs out.
* **Settings.** *Settings > Viewers and uploads* switches thumbnails on/off for everybody and sets the memory
  cache size (default 64 MB; 0 = no cache). Both apply immediately.

---

## 7. Viewing: Weasis and OHIF

Click **View** on a study (it needs the `visualize` permission). On the intranet both viewers are offered;
guests only get OHIF, for their one study (section 8).

**OHIF (browser, nothing to install).** Opens `/ohif/viewer?StudyInstanceUIDs=<uid>` in a new tab. The
portal relays Orthanc's OHIF plugin and the DICOMweb data it reads. OHIF runs inside the portal's own
origin, so it authenticates with the session cookie (the staff session, or the guest's invitation session).
Only the OHIF pages get a more permissive Content-Security-Policy (OHIF needs WebAssembly decoders, web
workers and inline styles); the rest of the portal keeps a strict one.

**Weasis (desktop, staff only).** Install [Weasis](https://weasis.org) on the workstation (its installers
register the `weasis://` link handler). The portal creates a short-lived token and a link of this form (shown
decoded):

```
weasis://$dicom:rs --url "http://10.0.0.5:8080/dicom-web" -r "studyUID=1.2.840..." -H "Authorization: Bearer <token>"
```

Weasis then reads `/dicom-web/...` on the portal with that token. The address inside the link is the one the
user is currently using (or *Settings > Network > Public address for Weasis*). It opens the **whole study**.
Weasis saves the images on the workstation, which is why it is **never** offered to guests: its link route does
not exist on the internet listener, and a Weasis token is ignored there.

**The read-only proxy.** `/dicom-web/*`, `/wado` and `/ohif/*` are forwarded to Orthanc with **GET only**,
after checking the staff session or token (and the `visualize` permission), or the guest's invitation session.
Paths are validated (no `..`), cookies and tokens are never sent to Orthanc, and patient data is marked
non-cacheable.

**What this does not do.** OHIF has to receive the pixel data to draw it, so the images do reach a guest's
browser memory, and the viewer has its own screenshot tools. The measures in section 8 stop *casual* leakage
(no loose DICOM files, no desktop viewer cache, counted downloads); they cannot stop a determined person who
is allowed to look at an image from copying what is on their screen. For stricter needs combine them with short
link lifetimes, 0 downloads, and the audit log.

---

## 8. Sharing a study with somebody outside (invitation links)

The internet side of the portal works **only by invitation**: there are no outside accounts, no search and no
list of studies. Each invitation opens **one study**.

**Set-up (once, by an administrator).** *Settings > Share links*: type the **public address** the tunnel
serves, for example `https://pacs.example.com`. Until it is set, *Share...* explains that sharing is not set up.
The same section holds the default validity (**48 hours**) and default number of downloads (**1**), and the
maximums nobody can exceed (720 hours, 20 downloads). Make sure the Cloudflare Tunnel points to port `8081`
(section 10).

**Creating an invitation (staff with the `share` permission, on the intranet).**

1. **Studies** tab, **Share...** on the study.
2. Choose **how many hours** the link works (default 48), **how many ZIP downloads** it allows (default 1; **0 =
   online viewing only**) and, if you wish, a note for your own records (for example "Dr. Smith, second opinion").
3. **Create link and password.** You get the **link** (`https://pacs.example.com/s/<48 random characters>`) and a
   generated **password** (`K7QX-M2PD-9RHW`: 12 characters without look-alikes such as `0/O` or `1/I/L`). Both
   have copy buttons. **The password is shown only now** and cannot be recovered; send it by a different
   channel than the link (a phone call, an SMS).
4. The **Shares** tab lists the invitations (yours; administrators see everyone's) with the study, who made
   it, time left, downloads used, how often it was opened, its status (active / expired / revoked) and a
   **Revoke** button. Revoking cuts off a guest who is looking at the study at that very moment.

**What the guest sees.** They open the link, a plain page asks for the password (capital letters, spaces and
dashes do not matter). With the right one they get a single simple page:

* the study details (patient, date, description, modalities) and its **series** (number, modality, description,
  body part, image count);
* **View online** (opens the study in OHIF in a new tab) and **Download (ZIP)**;
* **"This link expires in 1 d 23 h 14 min"** counting down (the server's clock is used, so a wrong clock on the
  guest's computer changes nothing) and **"Downloads left: 1 of 1"**.

**The rules.**

| Rule | Detail |
|---|---|
| Time | The link works until it expires (default 48 h). After that **everything stops**: viewing, details, download. |
| Downloads | Each ZIP download that starts uses one of the allowed ones (default 1). The count is a single atomic database update, so simultaneous clicks cannot get more than the limit. If Orthanc cannot even start the ZIP, the download is given back. |
| Running out of downloads | **Does not stop online viewing**: the guest keeps the viewer until the time expires. Only expiry or revoking stop it. |
| One study only | The viewer's data requests are checked against the shared study's *StudyInstanceUID*: searches must be narrowed to it, other studies are refused, and the portal **never hands out whole DICOM files** (a whole study, series or instance, or WADO-URI without a picture format) through the viewer address, because that would get around the download limit. The viewer only gets what it needs: searches, metadata, pixel frames, rendered pictures. The one way to receive the files is the counted ZIP. |
| Guessing | The link has 192 random bits and the password about 59, stored only as an **Argon2 hash**. Wrong passwords are throttled **per link** (5 failures lock that link for a few minutes) and **per address**; the figures follow *Settings > Sessions and sign-in security*. Unknown and wrong are answered the same way. |
| Session | A correct password gives a cookie (`HttpOnly`, `SameSite=Lax`, at most 12 hours, never longer than the link). Every request re-checks the link in the database, so expiry and revoking act immediately. |
| Records | The audit log records *share_created*, *share_unlock*, *share_unlock_failed*, *share_download* and *share_revoked* (never the password). |

**Honest limits.** An invitation is not an identity check: whoever has both the link and the password can
open it while it is valid. Keep lifetimes short, send the two parts separately, and use **0 downloads** when
viewing is enough. A guest who may look at an image can still photograph the screen. Expired and revoked
links stay in the list (and in the database) until the audit retention setting removes them.

---

## 9. Sending a study to a remote DICOM node

Orthanc calls the remote systems it can talk to **DICOM modalities**. Administrators manage them in the
portal's **Settings > DICOM nodes** tab: *Add node* (name, AE title, host, port), *Edit*, *Delete*, and **Test** (a DICOM
ping, C-ECHO). The portal stores them in Orthanc itself through its REST API; they survive an Orthanc restart
only if Orthanc runs with `"DicomModalitiesInDatabase": true` (set in `deploy/orthanc.json`). You can also keep
declaring them in Orthanc's `DicomModalities`. Do not forget to declare Orthanc's AE title and address on the
other side.

1. On the **Studies** tab, click **Send to...** on a study, pick the destination, **Send study**.
2. The portal asks Orthanc to run a C-STORE job (`POST /modalities/{alias}/store` with
   `"Synchronous": false`), stores the job id and answers immediately.
3. The **Transfers** tab shows each transfer with the live Orthanc job state
   (Pending / Running / Success / Failure) and progress, refreshing every 3 seconds while a job is active.
   Users see their own transfers; administrators see everyone's.

Needs the `send` permission. Destinations with `"AllowStore": false` are not offered. Every transfer is
written to the audit log.

---

## 10. Cloudflare Tunnel

The tunnel makes an **outbound** connection from your server to Cloudflare: no port forwarding, and
Cloudflare provides HTTPS.

1. Cloudflare dashboard, **Zero Trust**, **Networks**, **Tunnels**: create a tunnel (type *Cloudflared*).
2. Copy its **token** into `TUNNEL_TOKEN` (options A and B, Docker Compose), or follow
   `deploy/cloudflared-config.yml` for a locally-managed tunnel running on the host (option C).
3. Add a **public hostname** (e.g. `pacs.example.com`) with service **HTTP** pointing to the **internet
   port**: `portal:8081` (`localhost:8081` when cloudflared runs on the host). **Not 8080.**
4. In the portal, **Settings > Share links**, type that address (`https://pacs.example.com`) as the public
   address of the share links.
5. Share a study from the intranet and open the link from outside. Staff keep using `http://<server>:8080`.

Things to know:

* **Never publish port 8080 to the internet.** Even if you did, requests with Cloudflare headers are refused
  there, but the correct setup is to expose only 8081 (docker-compose does this).
* **Cloudflare Access** (an extra login in front of everything) is optional and **not** a good fit here:
  your guests are people you invite, who have no account to log in with. The link plus its password is the
  access control. Use Access only if all your guests can be given an Access identity.
* **Large images through the tunnel.** Streaming multi-hundred-MB studies to a remote viewer works, but a
  slow internet link is the limit; the intranet is faster for big CT/MR series.
* Consider a Cloudflare rate-limiting rule on `/api/share/*/unlock` as a second layer against password
  guessing (the portal already throttles per link and per address).

---

## 11. Security summary

Built in:

* **Intranet/internet separation** by listener *and* by Cloudflare headers (section 1): the internet port has
  no login, search, upload, send, share or admin routes at all, and the staff port refuses Cloudflare traffic.
* **Invitation links** (section 8): unguessable link + generated password (Argon2-hashed, shown once), throttled
  per link and per address, confined to one study, no whole DICOM files through the viewer address, atomic
  download counter, expiry and revocation checked on every request, own cookie separate from staff sessions.
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
* Viewer proxy is GET-only; neither staff nor guest credentials ever reach Orthanc.
* **Configuration is admin-only and intranet-only**: the settings and DICOM-node routes require the `admin`
  permission and are not registered on the internet-facing port (nor reachable with Cloudflare headers).
  Creating and revoking links needs the `share` permission (revoking someone else's needs `admin`).
* Uploaded logo/icon files are identified by content, size-limited, and SVGs must be free of scripts; they are
  served sandboxed. Only administrators on the intranet can change them.
* Settings are validated before they are saved; the Orthanc password is write-only and masked in logs; a new
  listener address is opened before the old one is released, so a typo cannot take the portal down.
* Audit log (logins with IP, uploads, downloads, sends, Weasis launches, user changes, **settings and
  DICOM-node changes**, **links created, opened, failed, downloaded and revoked**).

Operational advice:

* Change every default password; use long passwords for `admin`.
* **Back up** the `portal-data` volume (users, audit, transfers) and the Orthanc volume (images).
* Do not publish Orthanc's port 8042; keep the DICOM port 4242 on the intranet.
* `portal.db` holds the Orthanc password: restrict access to it and to its backups.
* Sessions and tokens live in memory: restarting the portal signs everybody out.
* The audit log grows over time: set *Keep the audit log for (days)* in Settings if your policy allows deleting
  old entries (the default keeps everything).
* There is no per-study access control for staff: every user with `read` sees every study. Guests see only the
  study they were invited to.
* OHIF is third-party code served from the portal's origin; keep Orthanc and its plugins up to date.
* Medical data is regulated (GDPR/HIPAA...). This tool gives you the controls; policies and risk assessment
  remain your responsibility.

---

## 12. API reference

All JSON. State-changing calls need the header `X-Requested-With: portal`.
"intranet" = only registered on the staff listener (default `:8080`); "internet" = only on the invitation listener
(default `:8081`).

**Staff (intranet)**

| Method & path | Permission | Purpose |
|---|---|---|
| `POST /api/login`, `POST /api/logout`, `GET /api/me` | none / session | Session (`/me` returns the permissions and `features`: `ohif`, `thumbnails`, `share`) |
| `GET /api/studies?patient_name=&patient_id=&accession_number=&description=&modality=&date_from=&date_to=&limit=&offset=` | read | Search studies (`modality=ct,rx`) |
| `GET /api/studies/{id}/series` | read | Series of a study |
| `GET /api/series?...&body_part=` | read | Search series |
| `GET /api/studies/{id}/thumbnail`, `GET /api/series/{id}/thumbnail` (`?size=64\|128\|256`) | visualize | Preview JPEG (404 = nothing to show) |
| `GET /api/weasis/link?study_uid=` | visualize | Build the `weasis://` link |
| `GET /dicom-web/*`, `GET /wado`, `GET /ohif/*` | visualize (session cookie or Weasis bearer token) | Read-only viewer proxy |
| `POST /api/upload` (raw body) | upload | Upload one file |
| `GET /api/studies/{id}/download[?dicomdir=true]` | download | ZIP of a study |
| `GET /api/series/{id}/download[?dicomdir=true]` | download | ZIP of a series |
| `GET /api/modalities` | send | DICOM destinations |
| `POST /api/studies/{id}/send` `{ "target": "alias" }` | send | Start sending a study |
| `GET /api/transfers` | send | Recent transfers + live job state |
| `POST /api/studies/{id}/share` `{ "hours": 48, "downloads": 1, "note": "" }` | share | Create an invitation: answers `{ id, url, password, expires_at, hours, max_downloads, label }`; **the only time the password is returned** |
| `GET /api/shares` | share | The caller's invitations (admins: everyone's) with status, downloads used, openings |
| `POST /api/shares/{id}/revoke` | share (creator) or admin | Revoke an invitation at once |
| `GET/POST /api/admin/users`, `PUT/DELETE /api/admin/users/{id}`, `GET /api/admin/audit` | admin | Users and audit log |
| `PUT/DELETE /api/admin/branding/{logo\|favicon}` (raw file body) | admin | Upload / remove the logo or the website icon |
| `GET/PUT /api/admin/settings` | admin | Read / save the settings (partial update; Orthanc password write-only) |
| `POST /api/admin/orthanc/test` | admin | "Test connection" (saved or typed values) |
| `GET /api/admin/nodes`, `PUT/DELETE /api/admin/nodes/{name}`, `POST /api/admin/nodes/{name}/echo` | admin | Manage DICOM nodes, DICOM ping |

**Guest (internet)**: no session login; the password of the link gives the `share_session` cookie.

| Method & path | Needs | Purpose |
|---|---|---|
| `GET /s/{token}` (also `/`) | none | The invitation page (`static/share.html` + `share.js`) |
| `GET /api/share/{token}/status` | none | `unavailable`, `expired`, `locked` or `open` |
| `POST /api/share/{token}/unlock` `{ "password": "..." }` | none (throttled) | Check the password, set the guest cookie. 400 wrong, 410 expired/revoked, 429 throttled |
| `GET /api/share/{token}/study` | guest cookie | Patient, study, series, server time, expiry, downloads `{max, used, left}`, viewer address |
| `GET /api/share/{token}/download` | guest cookie | ZIP of the shared study; uses one download (403 when none are left, 410 when over) |
| `POST /api/share/{token}/lock` | guest cookie | End this guest session |
| `GET /dicom-web/*`, `GET /wado`, `GET /ohif/*` | guest cookie | The viewer, confined to the shared study (no whole DICOM files) |

**Both listeners**

| Method & path | Permission | Purpose |
|---|---|---|
| `GET /api/public` | none | Branding: name, window title, logo, icon, login notice |
| `GET /branding/logo`, `GET /branding/favicon` | none | The uploaded logo / website icon (or the built-in mark) |
| `GET /healthz` | none | Liveness probe |

---

## 13. Project layout

```
Cargo.toml            dependencies
.env.example          every setting, documented
Dockerfile            container image
deploy/               option B: docker-compose.yml (preconfigured Orthanc + portal + tunnel), .env.example, orthanc.json; cloudflared-config.yml (tunnel run on the host, option C)
deploy/existing-orthanc/  option A: docker-compose.yml + .env.example (portal + tunnel, for an Orthanc you already have)
static/               staff UI: index.html + app.js; guest page: share.html + share.js; app.css (both), favicon.svg (the default mark); all embedded in the binary. The two HTML files are templates: title and icon are filled in per request
src/main.rs           start-up, background cleanup, graceful shutdown
src/config.rs         the few bootstrap values from the environment
src/settings.rs       every web-editable setting: defaults, validation, saved copy, roles
src/server.rs         runs / re-opens the two listeners while the program runs
src/error.rs          AppError -> HTTP status + JSON
src/permissions.rs    the seven permissions and the rules between them
src/db.rs             SQLite: users, audit log, transfers, share links, saved settings
src/auth.rs           passwords, sessions, tokens, Cloudflare detection, request guards (staff and guest)
src/sharing.rs        invitation rules: password and link generation, what a guest may read (with unit tests)
src/branding.rs       uploaded logo / website icon: content checks, SVG safety, memory copy
src/thumbnails.rs     chooses the preview image of a study/series, memory cache
src/orthanc.rs        Orthanc REST client (find, upload, archive, nodes/jobs, viewer proxy), reconfigurable live
src/models.rs         study / series / DICOM-node JSON shapes sent to the browser
src/util.rs           validation and helpers
src/state.rs          shared application state
src/routes/           session, studies, thumbnails, upload, download, send, viewer (Weasis + proxy), shares (staff: create/list/revoke), guest (invitation page and API), admin, settings, branding, router
```

---

## 14. Testing status, first-run checklist, troubleshooting

The code compiles without warnings on Rust 1.91, passes `cargo clippy --all-targets` with no warnings, is
formatted with `rustfmt` (see `rustfmt.toml`), and its dependencies were checked against the RustSec advisory
database (no known vulnerable version in `Cargo.lock`). The automated tests live in the **`tests/`** folder:

* `tests/server_tests.sh`: **336 checks** of the server against a fake Orthanc (`tests/mock_orthanc.py`):
  the two doors (staff portal vs invitation links only, Cloudflare headers refused on the intranet port), creating
  links with the default and the limited validity/downloads, the password gate and its throttling, what a guest
  sees, **the confinement to the shared study** (other studies, unfiltered searches, whole DICOM files, WADO
  without a picture type), the **counted downloads** (including 8 simultaneous clicks on a 3-download link),
  running out of downloads while viewing continues, expiry, revocation, search and modality filters,
  thumbnails, sending and job tracking, upload/download, branding including malicious files, live settings,
  login hardening, caps, ETag/headers, concurrent administrators, and **talking to Orthanc with an account, without
  one, and when it refuses the portal** (what is sent, what is not, the message for each case), and an
  administrator changing their own password (every other session of that account is signed out);
* `tests/ui_tests.js`: **74 checks** of both web pages in a simulated browser (jsdom): the Share dialog and the
  Shares tab, the guest page (password, study, series, countdown, downloads left, expiry, revoked, 0-download
  links, no OHIF), what each kind of staff account sees, search, the with/without-account choice in Settings;
* `cargo test`: **17 unit tests** (image validation; password and link generation; the rules that confine a guest
  to one study; account / no account and the wording of Orthanc refusals; the thumbnail cache; tokens and sessions).

Run them all with `tests/run_all.sh` (needs `python3`, `curl`, and `npm install` once inside `tests/`; build
first with `cargo build`; set `PORTAL_BIN=target/release/orthanc-portal` to test the release binary). The
portal has **not** been run against a real Orthanc, a real OHIF
plugin, a real Weasis or a real DICOM destination here, and the compose files were validated with
`docker compose config` but **never started** (no Docker daemon was available), so on first use please verify:

0. **Settings > Orthanc connection > Test connection** shows your Orthanc version, the DICOMweb and OHIF
   plugins and the number of DICOM nodes.
1. **Modality filter**: select CT and RX; results include only those modalities.
2. **Browser viewer**: *View, Browser viewer (OHIF)* loads images (needs the OHIF plugin with
   `"DataSource": "dicom-web"`).
3. **Weasis**: *View, Weasis* loads the study (Weasis installed, DICOMweb plugin enabled, portal address
   reachable from the workstation).
4. **DICOM nodes**: add your real node, press **Test** (it must answer), then use **Send to...** on a small study
   and watch it reach *Success* in **Transfers**.
5. **Share a study**: in *Settings > Share links* enter the public address, then *Share...* on a study. Open the
   link in a private window (or on a phone using mobile data), type the password: you must see only that
   study, *View online* must load the images and *Download (ZIP)* must give a file and lower the counter.
   Try a second download (refused), then *Revoke* in the *Shares* tab and reload the guest page.
6. **Thumbnails**: the Studies and Series tabs show pictures. Not every image can be rendered (for example some
   JPEG 2000 files depending on how your Orthanc was built); those get the modality placeholder.

| Symptom | Likely cause |
|---|---|
| Red "Orthanc:" strip under the top bar, or "Orthanc ... is not supported" in the log | Orthanc unreachable, wrong credentials, or older than 1.12.5 / no `HasExtendedFind`. Fix it in **Settings > Orthanc connection**; the portal keeps running meanwhile |
| The portal does not start: "cannot start the intranet listener ... address already in use" (or `docker compose up` says "port is already allocated") | Another program uses that port: pick a free one, see [Changing the ports](#changing-the-ports) |
| I changed the port and lost the page | Settings shows a link to the new address; if the new address is unusable the old one is kept. Last resort: start once with `RESET_SETTINGS=true` |
| A DICOM node I added disappears after restarting Orthanc | Orthanc is not running with `"DicomModalitiesInDatabase": true` |
| Saving settings says "Cannot listen on ..." | That address is already used by another program or is not an address of this machine |
| "The Orthanc server is unreachable" | Wrong `ORTHANC_URL`, Orthanc down, or firewall between them |
| "Orthanc refused the account '...' (wrong user name or password)" | Correct the account in **Settings > Orthanc connection** |
| "Orthanc asks for a user name and password, but the portal is set to connect without an account" | Your Orthanc has users: choose *With an account* and enter it, or let Orthanc accept this computer without one |
| "Orthanc refuses this machine (HTTP 403)" | Orthanc (or the proxy in front of it) only accepts a list of allowed computers and the portal's address is not in it. See the table *Which address does Orthanc see?* in section 3; in Docker it is not `127.0.0.1` |
| "Orthanc does not allow the account '...' to do this (HTTP 403)" | The account exists but lacks permissions in Orthanc, or the portal's address is not accepted |
| Search error mentioning `Limit`/`Since` or a tag | Orthanc refused that filter for your index; the message comes from Orthanc |
| No "Browser viewer" entry | OHIF plugin not loaded (see `GET /plugins` on Orthanc) or `OHIF_ENABLED=false` |
| OHIF opens but shows no images / 404 on `/studies/...` | The plugin uses its default `dicom-json` data source: set `"OHIF": {"DataSource": "dicom-web"}` |
| OHIF page stays blank | Check the browser console for blocked resources; OHIF pages use the relaxed policy in `security_headers` (`src/routes/mod.rs`) |
| Weasis starts but shows no images | DICOMweb plugin disabled; or the workstation cannot reach the portal address in the link; or the token expired (click View again) |
| "Send to..." says no destination | No DICOM node yet (add one in **Settings > DICOM nodes**), or all have *Send* blocked |
| A transfer ends in Failure | Orthanc's explanation is shown under the state; usually the remote node does not know Orthanc's AET or is unreachable (try a DICOM echo from Orthanc) |
| A visitor from the internet gets "This address is for the intranet only" | The tunnel points to 8080: change it to 8081 |
| *Share...* says "Sharing is not set up yet" | An administrator must enter the public address in *Settings > Share links* |
| The guest opens the link and sees "This link is not available" | The link was revoked, or the address is wrong or incomplete (it must be the whole `/s/<48 characters>`) |
| The guest sees "This link has expired" | The validity ran out; create a new link (the old one cannot be extended) |
| The guest lost the password | It cannot be recovered or shown again: revoke the link and create a new one |
| "Too many attempts" on the invitation page | Several wrong passwords: that link (or that address) is locked for a few minutes (*Settings > Sessions and sign-in security*) |
| The guest's *View online* button is grey | The OHIF plugin is not available in Orthanc (see *Requirements*); the ZIP download still works |
| The guest says the viewer opens but some images do not show | Some object types need whole DICOM files, which guests never get through the viewer address; use the ZIP, or Weasis on the intranet |
| A guest started a download and it did not finish | It still used one of the allowed downloads (a started download counts); create a new link with more downloads |
| Every thumbnail is a grey placeholder | Thumbnails switched off in Settings, the user lacks `visualize`, or Orthanc cannot render those images (check `GET /instances/{id}/rendered` on Orthanc) |
| Thumbnails are slow on the first view of a big study | Orthanc is decoding images; they load as you scroll and are then cached in memory (raise *Thumbnail cache* if you browse many studies) |
| The tab still shows the old icon after I changed it | Browsers cache favicons aggressively: reload the page, or close and reopen the tab |
| My logo is refused | Only PNG, JPEG, WebP, GIF or SVG up to 1 MB (icon: up to 256 KB). An SVG with scripts, event handlers or `<foreignObject>` is refused: export a plain SVG |
| My logo looks too small or cut | It is shown about 32 px high (48 px on the login page) and at most 220 px wide: use a wide image with a transparent background |
| "Too many failed attempts" for everybody in one office | The limit per *address* was reached (4 times the per-account limit). It clears after the lockout time; raise the numbers in *Settings > Sessions and sign-in security* if your staff share one address |
| The password page works on the intranet address but the cookie is refused through Cloudflare (or the reverse) | `COOKIE_SECURE=true` over plain HTTP, or `never` over HTTPS; keep `auto` |

---

## 15. Maintenance

Before publishing a change: `cargo fmt`, `cargo clippy --all-targets`, `cargo test`, `cargo build --release`.
Check the dependencies for known vulnerabilities regularly (for example monthly) with
[`cargo audit`](https://crates.io/crates/cargo-audit) and update with `cargo update` (patch-level updates are
compatible). The portal itself needs no maintenance task: sessions, tokens and old audit records are cleaned up
automatically (audit retention is a setting).

Known limits, by design: Orthanc credentials are stored in `portal.db` in clear (the portal must use them);
there is no per-study access control; sessions live in memory; the portal writes no access log of its own (the
audit log records sensitive actions only). Two dependency upgrades were deliberately left for a later,
separate step because they are major versions with API changes: `reqwest` 0.12 -> 0.13 and `argon2` 0.5 -> 0.6.
