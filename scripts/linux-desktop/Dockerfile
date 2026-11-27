ARG RUST_IMAGE
FROM ${RUST_IMAGE}
ARG BUILD_PACKAGES
ARG DEBIAN_SNAPSHOT=20261118T000000Z
# snapshot.debian.org is slow and sometimes refuses connections: retry each
# download and the whole update/install sequence.
RUN rm -f /etc/apt/sources.list.d/debian.sources \
    && printf 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/%s/ trixie main\n' "$DEBIAN_SNAPSHOT" > /etc/apt/sources.list \
    && printf 'Acquire::Retries "8";\nAcquire::http::Timeout "60";\nAcquire::https::Timeout "60";\n' > /etc/apt/apt.conf.d/80retries \
    && for attempt in 1 2 3; do \
         apt-get update \
         && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends $BUILD_PACKAGES \
            xvfb x11-utils x11-xserver-utils xauth xdotool wmctrl dbus-x11 at-spi2-core python3-pyatspi python3-dbus python3-gi \
            xfce4-session xfce4-panel xfwm4 xfce4-settings xfce4-notifyd libxfce4ui-utils \
            xdg-desktop-portal xdg-desktop-portal-gtk gnome-keyring libsecret-tools \
            pipewire pipewire-pulse wireplumber libspa-0.2-modules pulseaudio-utils \
            mesa-vulkan-drivers libgl1-mesa-dri fonts-dejavu-core adwaita-icon-theme imagemagick procps jq \
            faketime \
         && break; \
         [ "$attempt" = 3 ] && exit 1; sleep 20; \
       done \
    && test -e "/usr/lib/$(uname -m)-linux-gnu/faketime/libfaketime.so.1" \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 1000 qa
USER qa
WORKDIR /home/qa
