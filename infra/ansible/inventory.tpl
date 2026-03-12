[server]
${server_ip} ansible_user=alpine

[client]
${client_ip} ansible_user=alpine

[all:vars]
ansible_become_method=doas
ansible_ssh_private_key_file=~/.ssh/${key_name}.pem
