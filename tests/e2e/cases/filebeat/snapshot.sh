filebeat version 2>&1 | head -1; md5sum /etc/filebeat/filebeat.yml 2>&1 | cut -c1-34; ls /etc/apt/sources.list.d/
