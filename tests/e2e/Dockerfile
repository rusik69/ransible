FROM ubuntu:24.04
ENV container=podman DEBIAN_FRONTEND=noninteractive
RUN apt-get update -qq && apt-get install -y -qq systemd systemd-sysv dbus openssh-server sudo curl git rsync ca-certificates python3 util-linux-extra tzdata >/dev/null \
 && mkdir -p /run/sshd && useradd -m -s /bin/bash deploy \
 && echo 'deploy ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/deploy && mkdir -p /home/deploy/.ssh \
 && systemctl enable ssh >/dev/null 2>&1; \
    rm -f /etc/systemd/system/*.wants/getty* /lib/systemd/system/multi-user.target.wants/getty* 2>/dev/null; true
COPY key.pub /home/deploy/.ssh/authorized_keys
RUN chown -R deploy:deploy /home/deploy/.ssh && chmod 700 /home/deploy/.ssh && chmod 600 /home/deploy/.ssh/authorized_keys
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
