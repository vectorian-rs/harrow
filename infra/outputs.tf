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

output "run_harrow_server" {
  description = "Command to run Harrow server (paste on server instance)"
  value       = "cd ~/harrow && RUST_LOG=error ./target/release/harrow-server --bind 0.0.0.0 --port 3090"
}

output "run_axum_server" {
  description = "Command to run Axum server (paste on server instance)"
  value       = "cd ~/harrow && RUST_LOG=error ./target/release/axum-server --bind 0.0.0.0 --port 3091"
}

output "run_bench" {
  description = "Command to run full comparison (paste on client instance)"
  value       = "cd ~/harrow && SERVER_HOST=${aws_spot_instance_request.server.private_ip} ./scripts/compare-frameworks.sh --remote --bench-bin ~/mcp-load-tester/target/release/bench"
}

output "ansible_inventory" {
  description = "Ansible inventory (paste to infra/ansible/inventory.ini)"
  value = templatefile("${path.module}/ansible/inventory.tpl", {
    server_ip = aws_spot_instance_request.server.public_ip
    client_ip = aws_spot_instance_request.client.public_ip
    key_name  = var.key_name
  })
}
