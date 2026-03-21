import * as cdk from 'aws-cdk-lib';
import * as fs from 'fs';
import * as path from 'path';
import { GatewayStack } from './stack';

const app = new cdk.App();

// Read environment config from environments.json
const envFile = path.resolve(__dirname, '..', 'environments.json');
const environments = JSON.parse(fs.readFileSync(envFile, 'utf-8'));

// Select environment: -c environment=staging|prod (default: prod)
const envName = app.node.tryGetContext('environment') || 'prod';
const envConfig = environments[envName];
if (!envConfig) {
  throw new Error(`Unknown environment '${envName}'. Use -c environment=staging or -c environment=prod`);
}

const stackName = environments.stack_name || 'CCAG';

new GatewayStack(app, stackName, {
  env: {
    account: envConfig.account_id,
    region: environments.region,
  },
  desiredCount: envConfig.desired_count,
  ecrRepoName: environments.ecr_repo_name,
  domainName: envConfig.domain_name || undefined,
  hostedZoneName: envConfig.hosted_zone_name || undefined,
  certificateArn: envConfig.certificate_arn || undefined,
  adminUsers: envConfig.admin_users || undefined,
  adminPassword: envConfig.admin_password || undefined,
  imageTag: app.node.tryGetContext('imageTag') || undefined,
  imageDigest: app.node.tryGetContext('imageDigest') || undefined,
  rdsIamAuth: app.node.tryGetContext('rdsIamAuth') === 'true' || envConfig.rds_iam_auth === true,
  allowedCidrs: envConfig.allowed_cidrs || undefined,
});
