import * as cdk from 'aws-cdk-lib';
import * as acm from 'aws-cdk-lib/aws-certificatemanager';
import * as cloudwatch from 'aws-cdk-lib/aws-cloudwatch';
import * as cloudwatch_actions from 'aws-cdk-lib/aws-cloudwatch-actions';
import * as ec2 from 'aws-cdk-lib/aws-ec2';
import * as ecr from 'aws-cdk-lib/aws-ecr';
import * as ecs from 'aws-cdk-lib/aws-ecs';
import * as ecs_patterns from 'aws-cdk-lib/aws-ecs-patterns';
import * as elbv2 from 'aws-cdk-lib/aws-elasticloadbalancingv2';
import * as iam from 'aws-cdk-lib/aws-iam';
import * as rds from 'aws-cdk-lib/aws-rds';
import * as route53 from 'aws-cdk-lib/aws-route53';
import * as route53_targets from 'aws-cdk-lib/aws-route53-targets';

import * as sns from 'aws-cdk-lib/aws-sns';
import * as logs from 'aws-cdk-lib/aws-logs';
import { Construct } from 'constructs';

export interface GatewayStackProps extends cdk.StackProps {
  desiredCount: number;
  /** ECR repository name. If omitted, pulls from GHCR (ghcr.io/antkawam/claude-code-aws-gateway) */
  ecrRepoName?: string;
  /** Full domain name for the gateway, e.g. ccag.example.com */
  domainName?: string;
  /** Route53 hosted zone name, e.g. example.com */
  hostedZoneName?: string;
  /** ACM certificate ARN (optional — auto-created if domainName + hostedZoneName are set) */
  certificateArn?: string;
  /** Comma-separated OIDC subjects to bootstrap as admin */
  adminUsers?: string;
  /** Admin login password (default: 'admin' — override per environment) */
  adminPassword?: string;
  /** Image tag to deploy (default: latest). Used for both ECR and GHCR sources */
  imageTag?: string;
  /** ECR image digest (sha256:...) — preferred over imageTag to avoid no-op deploys. ECR only */
  imageDigest?: string;
  /** Use RDS IAM authentication instead of Secrets Manager password (default: false) */
  rdsIamAuth?: boolean;
  /** CIDR blocks allowed to reach the ALB (e.g. ["203.0.113.0/24"]). When unset, ALB is open to 0.0.0.0/0 */
  allowedCidrs?: string[];
}

export class GatewayStack extends cdk.Stack {
  constructor(scope: Construct, id: string, props: GatewayStackProps) {
    super(scope, id, props);

    // VPC
    const vpc = new ec2.Vpc(this, 'Vpc', {
      maxAzs: 2,
      natGateways: 1,
    });

    // RDS PostgreSQL
    const db = new rds.DatabaseInstance(this, 'Database', {
      engine: rds.DatabaseInstanceEngine.postgres({
        version: rds.PostgresEngineVersion.VER_16,
      }),
      instanceType: ec2.InstanceType.of(
        ec2.InstanceClass.T4G,
        ec2.InstanceSize.SMALL,
      ),
      vpc,
      vpcSubnets: { subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS },
      credentials: rds.Credentials.fromGeneratedSecret('proxy'),
      databaseName: 'proxy',
      allocatedStorage: 20,
      maxAllocatedStorage: 100,
      backupRetention: cdk.Duration.days(7),
      storageEncrypted: true,
      deletionProtection: true,
      removalPolicy: cdk.RemovalPolicy.SNAPSHOT,
      // IAM authentication — allows ECS task role to generate short-lived DB auth tokens
      iamAuthentication: props.rdsIamAuth ?? false,
      // Observability
      enablePerformanceInsights: true,
      monitoringInterval: cdk.Duration.seconds(60),
      cloudwatchLogsExports: ['postgresql'],
    });

    // Auto-rotate the database password every 30 days
    if (db.secret) {
      db.addRotationSingleUser({ automaticallyAfter: cdk.Duration.days(30) });
    }

    // ECS Cluster
    const cluster = new ecs.Cluster(this, 'Cluster', {
      vpc,
      containerInsightsV2: ecs.ContainerInsights.ENABLED,
    });

    // Task Definition
    const taskDef = new ecs.FargateTaskDefinition(this, 'TaskDef', {
      memoryLimitMiB: 1024,
      cpu: 512,
      runtimePlatform: {
        cpuArchitecture: ecs.CpuArchitecture.ARM64,
        operatingSystemFamily: ecs.OperatingSystemFamily.LINUX,
      },
    });

    // Bedrock access — need both foundation-model and inference-profile ARNs
    // Foundation models: arn:aws:bedrock:*::foundation-model/anthropic.claude*
    // Inference profiles: arn:aws:bedrock:*:ACCOUNT:inference-profile/*anthropic.claude*
    taskDef.taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: [
          'bedrock:InvokeModel',
          'bedrock:InvokeModelWithResponseStream',
          'bedrock:ListInferenceProfiles',
        ],
        resources: [
          'arn:aws:bedrock:*::foundation-model/anthropic.claude*',
          `arn:aws:bedrock:*:${this.account}:inference-profile/*anthropic.claude*`,
          '*', // ListInferenceProfiles requires wildcard resource
        ],
      }),
    );

    // SNS Publish — app-level notifications (budget alerts, etc.) to BYO SNS topics
    taskDef.taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: ['sns:Publish'],
        resources: ['*'],
      }),
    );

    // EventBridge PutEvents — app-level notifications to BYO event buses
    taskDef.taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: ['events:PutEvents'],
        resources: ['*'],
      }),
    );

    // Service Quotas: needed by endpoints admin to display per-endpoint RPM/TPM limits
    taskDef.taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: [
          'servicequotas:ListServiceQuotas',
          'servicequotas:GetServiceQuota',
        ],
        resources: ['*'],
      }),
    );

    // RDS IAM authentication — generate short-lived auth tokens instead of static passwords
    if (props.rdsIamAuth) {
      taskDef.taskRole.addToPrincipalPolicy(
        new iam.PolicyStatement({
          actions: ['rds-db:connect'],
          resources: [
            `arn:aws:rds-db:${this.region}:${this.account}:dbuser:${db.instanceResourceId}/proxy`,
          ],
        }),
      );
    }

    // Database connection env vars
    const oidcEnv: Record<string, string> = {};
    if (props.domainName) {
      oidcEnv.OIDC_AUDIENCE = props.domainName;
    }
    if (props.adminUsers) {
      oidcEnv.ADMIN_USERS = props.adminUsers;
    }
    if (props.adminPassword) {
      oidcEnv.ADMIN_PASSWORD = props.adminPassword;
    }

    const logGroup = new logs.LogGroup(this, 'AppLogGroup', {
      retention: logs.RetentionDays.ONE_MONTH,
      removalPolicy: cdk.RemovalPolicy.RETAIN,
    });

    // Image source: ECR (private) if ecrRepoName is set, otherwise GHCR (public)
    const ghcrImage = `ghcr.io/antkawam/claude-code-aws-gateway:${props.imageTag ?? 'latest'}`;
    const containerImage = props.ecrRepoName
      ? ecs.ContainerImage.fromEcrRepository(
          ecr.Repository.fromRepositoryName(this, 'EcrRepo', props.ecrRepoName),
          props.imageDigest ?? props.imageTag ?? 'latest',
        )
      : ecs.ContainerImage.fromRegistry(ghcrImage);
    const containerName = props.ecrRepoName ?? 'ccag';
    const container = taskDef.addContainer(containerName, {
      image: containerImage,
      logging: ecs.LogDrivers.awsLogs({
        streamPrefix: containerName,
        logGroup,
      }),
      environment: {
        PROXY_HOST: '0.0.0.0',
        PROXY_PORT: '8080',
        RUST_LOG: 'info',
        LOG_FORMAT: 'json',
        ...(props.rdsIamAuth ? { RDS_IAM_AUTH: 'true' } : {}),
        ...oidcEnv,
      },
      portMappings: [{ containerPort: 8080 }],
      healthCheck: {
        command: ['CMD-SHELL', 'curl -f http://localhost:8080/health || exit 1'],
        interval: cdk.Duration.seconds(15),
        timeout: cdk.Duration.seconds(5),
        retries: 3,
      },
    });

    // TLS certificate — use provided ARN, or auto-create via Route53 DNS validation
    let certificate: acm.ICertificate | undefined;
    let hostedZone: route53.IHostedZone | undefined;

    if (props.hostedZoneName) {
      hostedZone = route53.HostedZone.fromLookup(this, 'HostedZone', {
        domainName: props.hostedZoneName,
      });
    }

    if (props.certificateArn) {
      certificate = acm.Certificate.fromCertificateArn(this, 'Certificate', props.certificateArn);
    } else if (props.domainName && hostedZone) {
      certificate = new acm.Certificate(this, 'Certificate', {
        domainName: props.domainName,
        validation: acm.CertificateValidation.fromDns(hostedZone),
      });
    }

    // Fargate Service with ALB
    const service = new ecs_patterns.ApplicationLoadBalancedFargateService(
      this,
      'Service',
      {
        cluster,
        taskDefinition: taskDef,
        desiredCount: props.desiredCount,
        publicLoadBalancer: true,
        assignPublicIp: false,
        taskSubnets: { subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS },
        listenerPort: certificate ? 443 : 80,
        certificate,
        sslPolicy: certificate ? elbv2.SslPolicy.RECOMMENDED_TLS : undefined,
        redirectHTTP: !!certificate,
        // ECS Exec for DB access and debugging (replaces bastion instance)
        enableExecuteCommand: true,
      },
    );

    service.targetGroup.configureHealthCheck({
      path: '/health',
      interval: cdk.Duration.seconds(15),
      healthyThresholdCount: 2,
      unhealthyThresholdCount: 3,
    });

    // ALB ingress restriction — replace default 0.0.0.0/0 with specific CIDRs
    if (props.allowedCidrs && props.allowedCidrs.length > 0) {
      const listenerPort = certificate ? 443 : 80;
      const cfnSg = service.loadBalancer.connections.securityGroups[0]
        .node.defaultChild as ec2.CfnSecurityGroup;

      // Build CIDR-specific ingress rules to replace CDK's default 0.0.0.0/0
      const ingressRules: Array<{CidrIp: string; FromPort: number; ToPort: number; IpProtocol: string; Description: string}> = [];
      const ports = [listenerPort, ...(certificate ? [80] : [])];
      for (const cidr of props.allowedCidrs) {
        for (const port of ports) {
          ingressRules.push({
            CidrIp: cidr,
            FromPort: port,
            ToPort: port,
            IpProtocol: 'tcp',
            Description: `Allow from ${cidr} on port ${port}`,
          });
        }
      }

      cfnSg.addPropertyOverride('SecurityGroupIngress', ingressRules);
    }

    // Deployment circuit breaker — roll back after 3 failed task launches
    // instead of retrying indefinitely (default behavior causes 30+ min rollbacks).
    const cfnService = service.service.node.defaultChild as ecs.CfnService;
    cfnService.addPropertyOverride('DeploymentConfiguration', {
      DeploymentCircuitBreaker: { Enable: true, Rollback: true },
      MaximumPercent: 200,
      MinimumHealthyPercent: 100,
    });

    // ALB idle timeout — streaming Bedrock responses can have long pauses
    // (thinking models may take >60s before first chunk). Default 60s causes 504s.
    service.loadBalancer.setAttribute('idle_timeout.timeout_seconds', '900');

    // Target group deregistration delay — allow in-flight requests to complete
    // during deployments (streaming responses can be long-running)
    service.targetGroup.setAttribute('deregistration_delay.timeout_seconds', '120');

    // ECR pull access (only needed when using private ECR, not GHCR)
    if (props.ecrRepoName) {
      taskDef.executionRole!.addToPrincipalPolicy(
        new iam.PolicyStatement({
          actions: [
            'ecr:GetDownloadUrlForLayer',
            'ecr:BatchGetImage',
            'ecr:BatchCheckLayerAvailability',
          ],
          resources: [`arn:aws:ecr:${this.region}:${this.account}:repository/${props.ecrRepoName}`],
        }),
      );
      taskDef.executionRole!.addToPrincipalPolicy(
        new iam.PolicyStatement({
          actions: ['ecr:GetAuthorizationToken'],
          resources: ['*'],
        }),
      );
    }

    // Allow Fargate tasks to connect to RDS
    db.connections.allowDefaultPortFrom(service.service);

    // Database connection env vars
    container.addEnvironment('DATABASE_HOST', db.dbInstanceEndpointAddress);
    container.addEnvironment('DATABASE_PORT', db.dbInstanceEndpointPort);
    container.addEnvironment('DATABASE_NAME', 'proxy');
    container.addEnvironment('DATABASE_USER', 'proxy');

    if (!props.rdsIamAuth) {
      // Default path: inject password from Secrets Manager
      container.addSecret('DB_PASSWORD', ecs.Secret.fromSecretsManager(db.secret!, 'password'));
    }

    // Route53 alias record for the ALB
    if (props.domainName && hostedZone) {
      new route53.ARecord(this, 'AliasRecord', {
        zone: hostedZone,
        recordName: props.domainName,
        target: route53.RecordTarget.fromAlias(
          new route53_targets.LoadBalancerTarget(service.loadBalancer),
        ),
      });
    }

    // Scaling
    const scaling = service.service.autoScaleTaskCount({
      minCapacity: props.desiredCount,
      maxCapacity: props.desiredCount * 5,
    });

    scaling.scaleOnCpuUtilization('CpuScaling', {
      targetUtilizationPercent: 70,
    });

    scaling.scaleOnMemoryUtilization('MemoryScaling', {
      targetUtilizationPercent: 80,
    });

    // ---- Observability: CloudWatch Alarms ----
    const alarmTopic = new sns.Topic(this, 'AlarmTopic', {
      displayName: 'CCAG Alarms',
    });

    // ALB 5xx errors (the 504s we saw yesterday)
    new cloudwatch.Alarm(this, 'Alb5xxAlarm', {
      metric: service.loadBalancer.metrics.httpCodeElb(elbv2.HttpCodeElb.ELB_5XX_COUNT, {
        period: cdk.Duration.minutes(5),
        statistic: 'Sum',
      }),
      threshold: 5,
      evaluationPeriods: 1,
      alarmDescription: 'ALB is generating 5xx errors (e.g. 504 timeouts)',
      treatMissingData: cloudwatch.TreatMissingData.NOT_BREACHING,
    }).addAlarmAction(new cloudwatch_actions.SnsAction(alarmTopic));

    // Target 5xx errors (application errors)
    new cloudwatch.Alarm(this, 'Target5xxAlarm', {
      metric: service.loadBalancer.metrics.httpCodeTarget(elbv2.HttpCodeTarget.TARGET_5XX_COUNT, {
        period: cdk.Duration.minutes(5),
        statistic: 'Sum',
      }),
      threshold: 10,
      evaluationPeriods: 1,
      alarmDescription: 'Gateway targets returning 5xx errors',
      treatMissingData: cloudwatch.TreatMissingData.NOT_BREACHING,
    }).addAlarmAction(new cloudwatch_actions.SnsAction(alarmTopic));

    // Unhealthy targets
    new cloudwatch.Alarm(this, 'UnhealthyTargetsAlarm', {
      metric: service.targetGroup.metrics.unhealthyHostCount({
        period: cdk.Duration.minutes(1),
        statistic: 'Maximum',
      }),
      threshold: 1,
      evaluationPeriods: 2,
      alarmDescription: 'One or more ECS tasks failing health checks',
      treatMissingData: cloudwatch.TreatMissingData.NOT_BREACHING,
    }).addAlarmAction(new cloudwatch_actions.SnsAction(alarmTopic));

    // ALB target response time (p99)
    new cloudwatch.Alarm(this, 'HighLatencyAlarm', {
      metric: service.loadBalancer.metrics.targetResponseTime({
        period: cdk.Duration.minutes(5),
        statistic: 'p99',
      }),
      threshold: 120, // 120 seconds — streaming can be long, but this is extreme
      evaluationPeriods: 3,
      alarmDescription: 'p99 response time exceeding 120s for 15 min',
      treatMissingData: cloudwatch.TreatMissingData.NOT_BREACHING,
    }).addAlarmAction(new cloudwatch_actions.SnsAction(alarmTopic));

    // RDS CPU
    new cloudwatch.Alarm(this, 'DbCpuAlarm', {
      metric: db.metricCPUUtilization({ period: cdk.Duration.minutes(5) }),
      threshold: 80,
      evaluationPeriods: 3,
      alarmDescription: 'RDS CPU sustained above 80%',
      treatMissingData: cloudwatch.TreatMissingData.NOT_BREACHING,
    }).addAlarmAction(new cloudwatch_actions.SnsAction(alarmTopic));

    // RDS free storage
    new cloudwatch.Alarm(this, 'DbStorageAlarm', {
      metric: db.metricFreeStorageSpace({ period: cdk.Duration.minutes(5) }),
      threshold: 2 * 1024 * 1024 * 1024, // 2 GB
      comparisonOperator: cloudwatch.ComparisonOperator.LESS_THAN_OR_EQUAL_TO_THRESHOLD,
      evaluationPeriods: 1,
      alarmDescription: 'RDS free storage below 2 GB',
      treatMissingData: cloudwatch.TreatMissingData.BREACHING,
    }).addAlarmAction(new cloudwatch_actions.SnsAction(alarmTopic));

    // RDS database connections
    new cloudwatch.Alarm(this, 'DbConnectionsAlarm', {
      metric: db.metricDatabaseConnections({ period: cdk.Duration.minutes(5), statistic: 'Maximum' }),
      threshold: 80, // t4g.small supports ~120 connections
      evaluationPeriods: 2,
      alarmDescription: 'RDS connections approaching limit',
      treatMissingData: cloudwatch.TreatMissingData.NOT_BREACHING,
    }).addAlarmAction(new cloudwatch_actions.SnsAction(alarmTopic));

    // ---- Log-based Metric Filter & Alarm ----
    new cloudwatch.Alarm(this, 'AppErrorsAlarm', {
      metric: new logs.MetricFilter(this, 'AppErrorsFilter', {
        logGroup,
        filterPattern: logs.FilterPattern.anyTerm('ERROR', 'panic'),
        metricNamespace: 'CCAG',
        metricName: 'AppErrors',
        metricValue: '1',
        defaultValue: 0,
      }).metric({ period: cdk.Duration.minutes(5), statistic: 'Sum' }),
      threshold: 5,
      evaluationPeriods: 1,
      alarmDescription: 'Application errors (Bedrock failures, panics, parse errors)',
      treatMissingData: cloudwatch.TreatMissingData.NOT_BREACHING,
    }).addAlarmAction(new cloudwatch_actions.SnsAction(alarmTopic));

    new cdk.CfnOutput(this, 'AlarmTopicArn', {
      value: alarmTopic.topicArn,
      description: 'Subscribe with: aws sns subscribe --topic-arn <arn> --protocol email --notification-endpoint you@example.com',
    });

    // ---- CloudWatch Dashboard ----
    const dashboardName = id;
    const dashboard = new cloudwatch.Dashboard(this, 'Dashboard', {
      dashboardName,
    });

    // Row 1: ALB Traffic
    dashboard.addWidgets(
      new cloudwatch.GraphWidget({
        title: 'Request Count',
        width: 8,
        left: [service.loadBalancer.metrics.requestCount({ period: cdk.Duration.minutes(1), statistic: 'Sum' })],
      }),
      new cloudwatch.GraphWidget({
        title: 'Response Time (p50 / p99)',
        width: 8,
        left: [
          service.loadBalancer.metrics.targetResponseTime({ period: cdk.Duration.minutes(1), statistic: 'p50' }),
          service.loadBalancer.metrics.targetResponseTime({ period: cdk.Duration.minutes(1), statistic: 'p99' }),
        ],
      }),
      new cloudwatch.GraphWidget({
        title: 'ALB & Target Errors',
        width: 8,
        left: [
          service.loadBalancer.metrics.httpCodeElb(elbv2.HttpCodeElb.ELB_5XX_COUNT, { period: cdk.Duration.minutes(1), statistic: 'Sum' }),
          service.loadBalancer.metrics.httpCodeTarget(elbv2.HttpCodeTarget.TARGET_5XX_COUNT, { period: cdk.Duration.minutes(1), statistic: 'Sum' }),
          service.loadBalancer.metrics.httpCodeTarget(elbv2.HttpCodeTarget.TARGET_4XX_COUNT, { period: cdk.Duration.minutes(1), statistic: 'Sum' }),
        ],
      }),
    );

    // Row 2: ECS
    dashboard.addWidgets(
      new cloudwatch.GraphWidget({
        title: 'ECS CPU Utilization',
        width: 8,
        left: [service.service.metricCpuUtilization({ period: cdk.Duration.minutes(1) })],
      }),
      new cloudwatch.GraphWidget({
        title: 'ECS Memory Utilization',
        width: 8,
        left: [service.service.metricMemoryUtilization({ period: cdk.Duration.minutes(1) })],
      }),
      new cloudwatch.GraphWidget({
        title: 'Healthy / Unhealthy Hosts',
        width: 8,
        left: [
          service.targetGroup.metrics.healthyHostCount({ period: cdk.Duration.minutes(1) }),
          service.targetGroup.metrics.unhealthyHostCount({ period: cdk.Duration.minutes(1) }),
        ],
      }),
    );

    // Row 3: RDS
    dashboard.addWidgets(
      new cloudwatch.GraphWidget({
        title: 'RDS CPU Utilization',
        width: 8,
        left: [db.metricCPUUtilization({ period: cdk.Duration.minutes(1) })],
      }),
      new cloudwatch.GraphWidget({
        title: 'Database Connections',
        width: 8,
        left: [db.metricDatabaseConnections({ period: cdk.Duration.minutes(1) })],
      }),
      new cloudwatch.GraphWidget({
        title: 'Free Storage Space (GB)',
        width: 8,
        left: [new cloudwatch.MathExpression({
          expression: 'm1 / (1024 * 1024 * 1024)',
          usingMetrics: { m1: db.metricFreeStorageSpace({ period: cdk.Duration.minutes(5) }) },
          label: 'Free Storage (GB)',
        })],
      }),
    );

    // Row 4: Application
    dashboard.addWidgets(
      new cloudwatch.GraphWidget({
        title: 'Application Errors (from logs)',
        width: 12,
        left: [new cloudwatch.Metric({
          namespace: 'CCAG',
          metricName: 'AppErrors',
          statistic: 'Sum',
          period: cdk.Duration.minutes(5),
        })],
      }),
      new cloudwatch.SingleValueWidget({
        title: 'Current Metrics',
        width: 12,
        metrics: [
          service.loadBalancer.metrics.requestCount({ period: cdk.Duration.minutes(5), statistic: 'Sum', label: 'Requests (5m)' }),
          service.loadBalancer.metrics.httpCodeElb(elbv2.HttpCodeElb.ELB_5XX_COUNT, { period: cdk.Duration.minutes(5), statistic: 'Sum', label: 'ALB 5xx (5m)' }),
          db.metricDatabaseConnections({ period: cdk.Duration.minutes(5), statistic: 'Maximum', label: 'DB Connections' }),
        ],
      }),
    );

    new cdk.CfnOutput(this, 'DashboardUrl', {
      value: `https://${this.region}.console.aws.amazon.com/cloudwatch/home?region=${this.region}#dashboards:name=${dashboardName}`,
      description: 'CloudWatch Dashboard URL',
    });

    new cdk.CfnOutput(this, 'TaskRoleArn', {
      value: taskDef.taskRole.roleArn,
      description: 'ECS task role ARN for BYO SNS/EventBridge resource policies',
    });

    // Outputs
    const scheme = certificate ? 'https' : 'http';
    const host = props.domainName || service.loadBalancer.loadBalancerDnsName;

    new cdk.CfnOutput(this, 'GatewayUrl', {
      value: `${scheme}://${host}`,
    });

    new cdk.CfnOutput(this, 'AlbDnsName', {
      value: service.loadBalancer.loadBalancerDnsName,
    });

    new cdk.CfnOutput(this, 'PortalUrl', {
      value: `${scheme}://${host}/portal`,
    });

    if (db.secret) {
      new cdk.CfnOutput(this, 'DbSecretArn', {
        value: db.secret.secretArn,
        description: 'Secrets Manager ARN for database credentials. Retrieve: aws secretsmanager get-secret-value --secret-id <arn>',
      });
    }
  }
}
