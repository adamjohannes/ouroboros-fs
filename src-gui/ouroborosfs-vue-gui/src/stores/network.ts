import {ref} from 'vue'
import {defineStore} from 'pinia'

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
    const uploadLoading = ref(false)
    const healLoading = ref(false)
    const API_BASE = 'http://127.0.0.1:8000' // TODO: dynamically update this with envs

    /** Fetches the latest node status from the gateway */
    async function netmapGet() {
        nodesLoading.value = true
        try {
            const response = await fetch(`${API_BASE}/netmap/get`)
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
    async function fileList() {
        filesLoading.value = true
        try {
            const response = await fetch(`${API_BASE}/file/list`)
            if (!response.ok) throw new Error('Network response was not ok')

            files.value = await response.json()
            lastFilesUpdate.value = new Date().toLocaleTimeString()
        } catch (error) {
            console.error('Failed to fetch files:', error)
        } finally {
            filesLoading.value = false
        }
    }

    /** Uploads a file to the network */
    async function filePush(file: File) {
        uploadLoading.value = true
        try {
            const response = await fetch(`${API_BASE}/file/push`, {
                method: 'POST',
                headers: {
                    // Send raw bytes, not multipart-form
                    'Content-Type': 'application/octet-stream',
                    'X-Filename': file.name, // Send filename in a custom header
                },
                body: file, // The browser will stream the file body
            });

            if (!response.ok) {
                const errText = await response.text();
                throw new Error(`Push failed: ${errText}`);
            }

            // Refresh the file list to show the new file
            await fileList();

        } catch (error) {
            console.error('Failed to upload file:', error)
            alert(`Error uploading file: ${error}`);
        } finally {
            uploadLoading.value = false
        }
    }

    /** Triggers a browser download for a file */
    function filePull(filename: string) {
        // Ensure the filename is properly encoded for a URL path segment.
        const encodedFilename = encodeURIComponent(filename);
        const downloadUrl = `${API_BASE}/file/pull/${encodedFilename}`;

        // Create a temporary link element to trigger the download.
        const link = document.createElement('a');
        link.href = downloadUrl;

        link.setAttribute('download', filename);

        document.body.appendChild(link);
        link.click();

        // Clean up the temporary link
        document.body.removeChild(link);
    }

    /** Triggers a ring-wide NODE HEAL */
    async function networkHeal() {
        if (healLoading.value) return // Prevent concurrent heals
        healLoading.value = true
        try {
            const response = await fetch(`${API_BASE}/network/heal`, {
                method: 'POST',
            });

            const responseData = await response.json();

            if (!response.ok) {
                throw new Error(responseData.message || 'Heal request failed');
            }

            // Simple feedback to the user
            alert(`Heal response: ${responseData.message}`);

            // Refresh the node map
            await netmapGet();

        } catch (error) {
            console.error('Failed to trigger heal:', error);
            alert(`Error triggering heal: ${error}`);
        } finally {
            healLoading.value = false
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
        uploadLoading,
        healLoading,

        // Actions
        netmapGet,
        fileList,
        filePush,
        filePull,
        networkHeal,
    }
})
