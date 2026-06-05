# A systemd + sshd app host for the multi-host criterion-2 fixture. ubi9/ubi-init
# runs systemd as pid 1; we add sshd + python3 + curl so fraisier can host-pull
# (curl/sha256sum/ln) and restart the unit (systemctl) over SSH.
FROM registry.access.redhat.com/ubi9/ubi-init

RUN dnf -y install openssh-server python3 procps-ng iproute && dnf clean all \
    && systemctl enable sshd \
    && mkdir -p /root/.ssh /var/lib/app /releases \
    && chmod 700 /root/.ssh

COPY app.py /opt/app.py
COPY app.service /etc/systemd/system/app.service
RUN chmod 755 /opt/app.py
# The app unit is NOT enabled: fraisier's restart phase starts it after the first
# artifact is activated (before that there is no `current` symlink to read).
