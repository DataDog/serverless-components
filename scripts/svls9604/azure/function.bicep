targetScope = 'resourceGroup'

param name string
param storageName string
@secure()
param ddApiKey string
param ddSite string = 'datad0g.com'
param runId string
param location string = resourceGroup().location

resource storage 'Microsoft.Storage/storageAccounts@2023-05-01' = {
  name: storageName
  location: location
  tags: { svls9604: 'true', 'svls9604-run': runId }
  sku: { name: 'Standard_LRS' }
  kind: 'StorageV2'
  properties: { minimumTlsVersion: 'TLS1_2' }
}

resource plan 'Microsoft.Web/serverfarms@2024-11-01' = {
  name: '${name}-plan'
  location: location
  kind: 'functionapp'
  sku: { name: 'Y1', tier: 'Dynamic' }
  properties: { reserved: true }
}

resource app 'Microsoft.Web/sites@2024-11-01' = {
  name: name
  location: location
  kind: 'functionapp,linux'
  tags: {
    svls9604: 'true'
    'svls9604-run': runId
    runtime: 'node'
    'deployment-model': 'compat'
  }
  properties: {
    httpsOnly: true
    serverFarmId: plan.id
    siteConfig: {
      linuxFxVersion: 'NODE|20'
      appSettings: [
        { name: 'AzureWebJobsStorage', value: 'DefaultEndpointsProtocol=https;AccountName=${storage.name};AccountKey=${storage.listKeys().keys[0].value};EndpointSuffix=${environment().suffixes.storage}' }
        { name: 'FUNCTIONS_EXTENSION_VERSION', value: '~4' }
        { name: 'FUNCTIONS_WORKER_RUNTIME', value: 'node' }
        { name: 'WEBSITE_NODE_DEFAULT_VERSION', value: '~20' }
        { name: 'SCM_DO_BUILD_DURING_DEPLOYMENT', value: 'true' }
        { name: 'ENABLE_ORYX_BUILD', value: 'true' }
        { name: 'DD_API_KEY', value: ddApiKey }
        { name: 'DD_SITE', value: ddSite }
        { name: 'DD_ENV', value: 'svls9604-${runId}' }
        { name: 'DD_SERVICE', value: name }
      ]
    }
  }
}

output hostname string = app.properties.defaultHostName
output resourceId string = app.id
