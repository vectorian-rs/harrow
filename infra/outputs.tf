output "server_public_ip" {
  description = "Server instance public IP"
  value       = aws_spot_instance_request.server.public_ip
}

output "server_private_ip" {
  description = "Server instance private IP (use from client)"
  value       = aws_spot_instance_request.server.private_ip
}

output "client_public_ip" {
  description = "Client instance public IP"
  value       = aws_spot_instance_request.client.public_ip
}

output "ssh_server" {
  description = "SSH command for server instance"
  value       = "ssh -i ~/.ssh/${var.key_name}.pem alpine@${aws_spot_instance_request.server.public_ip}"
}

output "ssh_client" {
  description = "SSH command for client instance"
  value       = "ssh -i ~/.ssh/${var.key_name}.pem alpine@${aws_spot_instance_request.client.public_ip}"
}

output "ecr_serde_bench_server" {
  description = "ECR repo URL for serde-bench-server"
  value       = module.ecr_serde_bench_server.ecr-repository-url
}

output "ecr_axum_serde_server" {
  description = "ECR repo URL for axum-serde-server"
  value       = module.ecr_axum_serde_server.ecr-repository-url
}

output "run_bench" {
  description = "Command to run serde-bench (paste on client instance)"
  value       = "serde-bench --server-host ${aws_spot_instance_request.server.private_ip} --client-host ${aws_spot_instance_request.client.private_ip}"
}

output "ansible_inventory" {
  description = "Ansible inventory (paste to infra/ansible/inventory.ini)"
  value = templatefile("${path.module}/ansible/inventory.tpl", {
    server_ip = aws_spot_instance_request.server.public_ip
    client_ip = aws_spot_instance_request.client.public_ip
    key_name  = var.key_name
  })
}
