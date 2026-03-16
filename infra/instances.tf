# ---------------------------------------------------------------------------
# Spot instance requests — server + client
# ---------------------------------------------------------------------------

resource "aws_spot_instance_request" "server" {
  ami                    = local.ami
  instance_type          = var.instance_type
  key_name               = var.key_name
  vpc_security_group_ids = [aws_security_group.bench.id]
  availability_zone      = local.az
  placement_group        = aws_placement_group.bench.name
  iam_instance_profile   = aws_iam_instance_profile.bench.name

  spot_type            = "one-time"
  wait_for_fulfillment = true

  root_block_device {
    volume_size = 30
    volume_type = "gp3"
  }

  tags = merge(local.common_tags, { Name = "harrow-bench-server" })
}

resource "aws_spot_instance_request" "client" {
  ami                    = local.ami
  instance_type          = var.instance_type
  key_name               = var.key_name
  vpc_security_group_ids = [aws_security_group.bench.id]
  availability_zone      = local.az
  placement_group        = aws_placement_group.bench.name
  iam_instance_profile   = aws_iam_instance_profile.bench.name

  spot_type            = "one-time"
  wait_for_fulfillment = true

  root_block_device {
    volume_size = 30
    volume_type = "gp3"
  }

  tags = merge(local.common_tags, { Name = "harrow-bench-client" })
}
