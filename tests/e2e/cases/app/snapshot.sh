cat /opt/myapp/app.conf /etc/motd
grep APP_ENV /etc/environment
stat -c "%U %G %a" /opt/myapp /opt/myapp/app.conf
id appsvc
git config --global --add safe.directory '*'; sudo git -C /opt/myapp/hello log --oneline | head -1
