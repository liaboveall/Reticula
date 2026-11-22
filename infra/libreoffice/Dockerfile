FROM debian:trixie-slim

RUN printf '%s\n' 'deb http://deb.debian.org/debian trixie-backports main' > /etc/apt/sources.list.d/trixie-backports.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        fonts-dejavu-core=2.37-8 \
        locales=2.41-12+deb13u5 \
    && apt-get install -y --no-install-recommends -t trixie-backports \
        libreoffice-calc-nogui=4:26.8.2.2-2~bpo13+1 \
    && sed -i 's/^# *en_US.UTF-8 UTF-8/en_US.UTF-8 UTF-8/' /etc/locale.gen \
    && locale-gen \
    && rm -rf /var/lib/apt/lists/*

ENV HOME=/tmp \
    LANG=en_US.UTF-8 \
    LC_ALL=en_US.UTF-8 \
    SAL_USE_VCLPLUGIN=svp

WORKDIR /work

CMD ["soffice", "--version"]
