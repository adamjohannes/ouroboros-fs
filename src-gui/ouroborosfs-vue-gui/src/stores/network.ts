import { ref } from 'vue'
import { defineStore } from 'pinia'

// Matches the `FileInfo` struct in gateway.rs
export interface FileItem {
    name: string
    start: number
    size: number
}

// Matches the `NodeStatus` map in gateway.rs
export type NodeMap = Record<string, 'Alive' | 'Dead'>

export const useNetworkStore = defineStore('network', () => {
    // State
    const nodes = ref<NodeMap>({})
    const files = ref<FileItem[]>([])
    const nodesLoading = ref(false)
    const filesLoading = ref(false)
    const lastFilesUpdate = ref<string>('')
    const lastNodesUpdate = ref<string>('')
    const API_BASE = 'http://127.0.0.1:8000/api' // TODO: dynamically update this with envs

    /** Fetches the latest node status from the gateway */
    async function fetchNodes() {
        nodesLoading.value = true
        try {
            const response = await fetch(`${API_BASE}/nodes`)
            if (!response.ok) throw new Error('Network response was not ok')

            nodes.value = await response.json()
            lastNodesUpdate.value = new Date().toLocaleTimeString()
        } catch (error) {
            console.error('Failed to fetch nodes:', error)
        } finally {
            nodesLoading.value = false
        }
    }

    /** Fetches the latest file list from the gateway */
    async function fetchFiles() {
        filesLoading.value = true
        try {
            const response = await fetch(`${API_BASE}/files`)
            if (!response.ok) throw new Error('Network response was not ok')

            files.value = await response.json()
            lastFilesUpdate.value = new Date().toLocaleTimeString()
        } catch (error) {
            console.error('Failed to fetch files:', error)
        } finally {
            filesLoading.value = false
        }
    }

    return {
        // State
        nodes,
        files,
        nodesLoading,
        filesLoading,
        lastFilesUpdate,
        lastNodesUpdate,
        // Actions
        fetchNodes,
        fetchFiles,
    }
})
