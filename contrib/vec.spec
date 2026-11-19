Name:           vec
Version:        0.1.0
Release:        1%{?dist}
Summary:        Semantic file search — find files by meaning
License:        MIT
URL:            https://github.com/kvzp/vec
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  sqlite-devel
BuildRequires:  pkg-config
BuildRequires:  systemd-rpm-macros

Requires:       sqlite-libs
Recommends:     vec-model-base

%description
locate finds files by name. vec finds files by meaning.

vec indexes the filesystem with CPU-based embeddings (pure-Rust ONNX, no
Python, no cloud), stores only vectors and byte offsets in a central SQLite
database, and enforces per-user access control via filesystem permissions.

After 'dnf install vec vec-model-base', open a terminal and type
'vec "authentication middleware"' — it works with no manual setup.

%prep
%autosetup

%build
cargo build --release -p vec-cli --no-default-features --features system-sqlite

%install
# Binary
install -D -m 0755 target/release/vec %{buildroot}%{_bindir}/vec

# Config template (all defaults commented out)
install -D -m 0644 contrib/vec.conf %{buildroot}%{_sysconfdir}/vec.conf

# systemd units (vendor path — not /etc/systemd/system)
install -D -m 0644 contrib/vec-updatedb.service \
    %{buildroot}%{_unitdir}/vec-updatedb.service
install -D -m 0644 contrib/vec-updatedb.timer \
    %{buildroot}%{_unitdir}/vec-updatedb.timer
install -D -m 0644 contrib/vec-watch.service \
    %{buildroot}%{_unitdir}/vec-watch.service
# Optional embedding daemon (socket-activated): shipped disabled
install -D -m 0644 contrib/vec-embed.service \
    %{buildroot}%{_unitdir}/vec-embed.service
install -D -m 0644 contrib/vec-embed.socket \
    %{buildroot}%{_unitdir}/vec-embed.socket

# sysctl: raise inotify watch limit for real-time indexing
install -D -m 0644 contrib/99-vec.conf \
    %{buildroot}%{_prefix}/lib/sysctl.d/99-vec.conf

# Userland units for users to copy, shipped as %doc (-> %{_pkgdocdir}/contrib/user).
# Defaults and file locations are compiled in: nothing else to install.
mkdir -p userdoc/contrib && cp -pr contrib/user userdoc/contrib/

%post
%systemd_post vec-updatedb.timer vec-watch.service

%preun
%systemd_preun vec-updatedb.timer vec-watch.service vec-embed.socket vec-embed.service

%postun
%systemd_postun_with_restart vec-watch.service vec-embed.service

%files
%license LICENSE
%doc README.md userdoc/contrib
%{_bindir}/vec
%config(noreplace) %{_sysconfdir}/vec.conf
%{_unitdir}/vec-updatedb.service
%{_unitdir}/vec-updatedb.timer
%{_unitdir}/vec-watch.service
%{_unitdir}/vec-embed.service
%{_unitdir}/vec-embed.socket
%{_prefix}/lib/sysctl.d/99-vec.conf

%changelog
* Tue Mar 03 2026 Gilles <gdevos@gmail.com> - 0.1.0-1
- Initial packaging
