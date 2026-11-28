grep -vE "^#|^$" /etc/ssh/sshd_config
cat /etc/timezone
echo DBG; timedatectl 2>&1 | head -3; readlink /etc/localtime; ls -l /etc/timezone; command -v dpkg-reconfigure
tail -n 4 /var/log/dpkg.log | cut -c21-; ls -l --time-style=full-iso /etc/localtime /etc/timezone; journalctl -u systemd-timedated --no-pager 2>&1 | tail -5 | cut -c17-
