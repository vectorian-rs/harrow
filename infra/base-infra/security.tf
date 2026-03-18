# ---------------------------------------------------------------------------
# Security group — SSH from user IP, bench port between instances
# ---------------------------------------------------------------------------

resource "aws_security_group" "bench" {
  name        = "harrow-bench"
  description = "Harrow benchmark: SSH + inter-instance bench traffic"

  tags = merge(local.common_tags, { Name = "harrow-bench" })
}

# SSH from caller's public IP
resource "aws_vpc_security_group_ingress_rule" "ssh" {
  security_group_id = aws_security_group.bench.id
  description       = "SSH from deployer IP"
  ip_protocol       = "tcp"
  from_port         = 22
  to_port           = 22
  cidr_ipv4         = local.my_ip
}

# Bench port between instances in the same SG
resource "aws_vpc_security_group_ingress_rule" "bench_port" {
  security_group_id            = aws_security_group.bench.id
  description                  = "Bench port between instances"
  ip_protocol                  = "tcp"
  from_port                    = 3000
  to_port                      = 3100
  referenced_security_group_id = aws_security_group.bench.id
}

# OTLP port (4318) between instances (server -> client for Phase C)
resource "aws_vpc_security_group_ingress_rule" "otlp_port" {
  security_group_id            = aws_security_group.bench.id
  description                  = "OTLP port between instances"
  ip_protocol                  = "tcp"
  from_port                    = 4318
  to_port                      = 4318
  referenced_security_group_id = aws_security_group.bench.id
}

# Bench port from deployer IP (for health checks from laptop)
resource "aws_vpc_security_group_ingress_rule" "bench_port_deployer" {
  security_group_id = aws_security_group.bench.id
  description       = "Bench port from deployer IP"
  ip_protocol       = "tcp"
  from_port         = 3000
  to_port           = 3100
  cidr_ipv4         = local.my_ip
}

# All outbound
resource "aws_vpc_security_group_egress_rule" "all_out" {
  security_group_id = aws_security_group.bench.id
  ip_protocol       = "-1"
  cidr_ipv4         = "0.0.0.0/0"
}
