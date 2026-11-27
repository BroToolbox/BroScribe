ARG RUST_IMAGE
FROM ${RUST_IMAGE}
ARG BUILD_PACKAGES
ARG DEBIAN_SNAPSHOT=20261118T000000Z
RUN rm -f /etc/apt/sources.list.d/debian.sources \
    && printf 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/%s/ trixie main\n' "$DEBIAN_SNAPSHOT" > /etc/apt/sources.list \
    && apt-get -o Acquire::Retries=3 update \
    && DEBIAN_FRONTEND=noninteractive apt-get -o Acquire::Retries=3 install -y --no-install-recommends $BUILD_PACKAGES \
       xvfb x11-utils xauth xdotool wmctrl dbus-x11 at-spi2-core python3-pyatspi python3-dbus python3-gi \
       xfce4-session xfce4-panel xfwm4 xfce4-settings xfce4-notifyd libxfce4ui-utils \
       xdg-desktop-portal xdg-desktop-portal-gtk gnome-keyring libsecret-tools \
       pipewire pipewire-pulse wireplumber libspa-0.2-modules pulseaudio-utils \
       mesa-vulkan-drivers libgl1-mesa-dri fonts-dejavu-core adwaita-icon-theme imagemagick procps jq \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 1000 qa
USER qa
WORKDIR /home/qa
