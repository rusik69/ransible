grep -vE "^#|^$" /etc/ssh/sshd_config
cat /etc/timezone
echo DBG; timedatectl 2>&1 | head -3; readlink /etc/localtime; ls -l /etc/timezone; command -v dpkg-reconfigure
