targetScope = 'resourceGroup'

param name string
param appEnvId string
param appImage string
param agentImage string
param registryServer string
param registryUsername string
@secure()
param registryPassword string
@secure()
param ddApiKey string
param ddSite string = 'datad0g.com'
param runtime string
param sidecar bool
param minReplicas int
param runId string
param location string = resourceGroup().location

var commonEnv = [
  { name: 'DD_API_KEY', value: ddApiKey }
  { name: 'DD_SITE', value: ddSite }
  { name: 'DD_ENV', value: 'svls9604-${runId}' }
  { name: 'DD_SERVICE', value: name }
  { name: 'DD_SERVERLESS_DIAGNOSTIC_INFO', value: 'true' }
  { name: 'DD_LOG_LEVEL', value: 'debug' }
  { name: 'DD_AZURE_SUBSCRIPTION_ID', value: subscription().subscriptionId }
  { name: 'DD_AZURE_RESOURCE_GROUP', value: resourceGroup().name }
]

resource app 'Microsoft.App/containerApps@2024-03-01' = {
  name: name
  location: location
  tags: {
    svls9604: 'true'
    'svls9604-run': runId
    runtime: runtime
    'deployment-model': sidecar ? 'sidecar' : 'in-container'
  }
  properties: {
    managedEnvironmentId: appEnvId
    configuration: {
      ingress: {
        external: true
        targetPort: 8080
        transport: 'auto'
      }
      registries: [
        {
          server: registryServer
          username: registryUsername
          passwordSecretRef: 'registry-password'
        }
      ]
      secrets: [
        { name: 'registry-password', value: registryPassword }
      ]
    }
    template: {
      containers: concat([
        {
          name: 'app'
          image: appImage
          resources: { cpu: json('0.5'), memory: '1Gi' }
          env: sidecar ? [
            { name: 'DD_ENV', value: 'svls9604-${runId}' }
            { name: 'DD_SERVICE', value: name }
          ] : commonEnv
        }
      ], sidecar ? [
        {
          name: 'datadog-sidecar'
          image: agentImage
          resources: { cpu: json('0.5'), memory: '1Gi' }
          env: commonEnv
        }
      ] : [])
      scale: {
        minReplicas: minReplicas
        maxReplicas: 100
      }
    }
  }
}

output fqdn string = app.properties.configuration.ingress.fqdn
output resourceId string = app.id
